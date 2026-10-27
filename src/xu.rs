// SPDX-License-Identifier: MIT
//! k1cam firmware protocol over the UVC Extension Unit (same as `uvcdctl firmware`).
//! Linux uses uvcvideo's XU ioctl; macOS sends the control transfers with libusb.
//! Windows binds the UVC interface to usbvideo, which libusb cannot reach, so it
//! is not supported there.

use crate::bundle::Bundle;
use crate::job::Job;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

#[cfg(all(not(windows), not(target_os = "linux")))]
use rusb::{DeviceHandle, GlobalContext};

#[cfg(all(test, target_os = "linux"))]
pub use v4l::UVCIOC_CTRL_QUERY;

/// On Linux uvcvideo owns the control interface, so raw libusb control transfers
/// are refused (EBUSY). The driver's own XU ioctl on /dev/videoN is used instead.
#[cfg(target_os = "linux")]
mod v4l {
    #[repr(C)]
    struct Query {
        unit: u8,
        selector: u8,
        query: u8,
        size: u16,
        data: *mut u8,
    }

    use crate::linux::{ioc, IOC_READ, IOC_WRITE};

    pub const UVCIOC_CTRL_QUERY: u32 = ioc(IOC_READ | IOC_WRITE, b'u', 0x21, std::mem::size_of::<Query>());

    pub fn query(f: &std::fs::File, unit: u8, selector: u8, query: u8, data: &mut [u8]) -> std::io::Result<()> {
        let mut q = Query { unit, selector, query, size: data.len() as u16, data: data.as_mut_ptr() };
        crate::linux::ioctl(f, UVCIOC_CTRL_QUERY, &mut q).map(|_| ())
    }

    /// The uvcvideo node that answers a GET_LEN of 60 on the command selector.
    pub fn find() -> Result<std::fs::File, String> {
        let mut denied = false;
        for i in 0..64 {
            match std::fs::OpenOptions::new().read(true).write(true).open(format!("/dev/video{i}")) {
                Ok(f) => {
                    let mut len = [0u8; 2];
                    if query(&f, super::UNIT as u8, super::SEL_COMMAND as u8, 0x85, &mut len).is_ok()
                        && u16::from_le_bytes(len) as usize == super::FRAME
                    {
                        return Ok(f);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
                Err(_) => {}
            }
        }
        Err(if denied {
            "no camera found; /dev/video* is not accessible (run as root or add yourself to the video group)".into()
        } else {
            "no k1cam camera with the firmware protocol found among /dev/video*".into()
        })
    }
}

/// One query to k1cam's Extension Unit through uvcvideo; the image controls
/// live there too.
#[cfg(target_os = "linux")]
pub fn xu_query(f: &std::fs::File, selector: u8, query: u8, data: &mut [u8]) -> std::io::Result<()> {
    v4l::query(f, UNIT as u8, selector, query, data)
}

// Unofficial FOSS VID and "K1" PID; retain the previous firmware identity
// so an update can cross the change in either direction (including rollback).
const CAM_VID: u16 = 0xf055;
const CAM_PID: u16 = 0x4b31;
const LEGACY_CAM_VID: u16 = 0x1d6b;
const LEGACY_CAM_PID: u16 = 0x0110;

/// Every USB identity of a camera running k1cam firmware (also its console's).
pub const CAMERA_IDS: [(u16, u16); 2] = [(CAM_VID, CAM_PID), (LEGACY_CAM_VID, LEGACY_CAM_PID)];

#[cfg(all(not(windows), not(target_os = "linux")))]
fn camera_id(vid: u16, pid: u16) -> bool {
    CAMERA_IDS.contains(&(vid, pid))
}
const UNIT: u16 = 4;
const SEL_COMMAND: u16 = 26;
const SEL_DATA: u16 = 27;
const SEL_STATUS: u16 = 28;
const FRAME: usize = 60;
const DATA_MAX: usize = 44;

const OP_INFO: u8 = 1;
const OP_READ: u8 = 2;
const OP_BEGIN_UPGRADE: u8 = 3;
const OP_ABORT: u8 = 4;
const OP_BEGIN_DUMP: u8 = 5;
const OP_FINISH_UPGRADE: u8 = 6;
const OP_REBOOT: u8 = 7;
const OP_END_DUMP: u8 = 8;

const TARGET_IMAGE: u8 = 0;
const TARGET_INFO: u8 = 1;
const TARGET_MANIFEST: u8 = 2;

const PH_ACCEPTED: u8 = 3;
const PH_VALIDATING: u8 = 5;
const PH_ERASING: u8 = 6;
const PH_PROGRAMMING: u8 = 7;
const PH_VERIFYING: u8 = 8;
const PH_STAGED: u8 = 9;
const PH_ERROR: u8 = 2;
const PH_READING: u8 = 12;
const PH_READY: u8 = 13;

const FW_FLASH_SIZE: u32 = 8 * 1024 * 1024;

type R<T> = Result<T, String>;

pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn put32(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
}

pub fn record_value<'a>(rec: &'a str, key: &str) -> Option<&'a str> {
    rec.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

pub fn command_frame(op: u8, target: u8, flags: u8, seq: u32, session: u32, offset: u32, length: u32) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    f[..4].copy_from_slice(b"K1FW");
    f[4] = 1;
    f[5] = op;
    f[6] = target;
    f[7] = flags;
    put32(&mut f[8..], seq);
    put32(&mut f[12..], session);
    put32(&mut f[16..], offset);
    put32(&mut f[20..], length);
    f
}

pub fn data_frame(session: u32, offset: u32, seq: u32, payload: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    put32(&mut f[0..], session);
    put32(&mut f[4..], offset);
    put32(&mut f[8..], seq);
    f[12] = payload.len() as u8;
    f[16..16 + payload.len()].copy_from_slice(payload);
    let crc = crc16(crc16(0xffff, &f[..14]), &f[16..16 + payload.len()]);
    f[14..16].copy_from_slice(&crc.to_le_bytes());
    f
}

fn phase_name(p: u8) -> &'static str {
    match p {
        0 => "idle",
        1 => "complete",
        2 => "error",
        3 => "accepted",
        4 => "receiving",
        5 => "validating",
        6 => "erasing",
        7 => "programming",
        8 => "verifying",
        9 => "staged",
        10 => "rebooting",
        11 => "aborted",
        12 => "reading",
        13 => "ready",
        _ => "unknown",
    }
}

fn error_text(code: u8, detail: u32) -> String {
    let hint = if code == 12 { " (not enough free RAM for a consistent dump; use the live option)" } else { "" };
    format!("camera error {code}, detail {detail}{hint}")
}

#[cfg(windows)]
pub struct Camera;

#[cfg(windows)]
impl Camera {
    fn unsupported<T>() -> R<T> {
        Err("A/B update over the UVC extension unit is not available on Windows yet. \
             Use Linux or macOS, or the cloner full flash."
            .into())
    }
    pub fn present() -> bool {
        false
    }
    pub fn open() -> R<Self> {
        Self::unsupported()
    }
    pub fn info(&mut self) -> R<String> {
        Self::unsupported()
    }
    pub fn dump(&mut self, _: &Job, _: bool) -> R<(Vec<u8>, String)> {
        Self::unsupported()
    }
    pub fn upgrade(&mut self, _: &Job, _: &Bundle, _: bool) -> R<String> {
        Self::unsupported()
    }
    pub fn reboot(&mut self) -> R<()> {
        Self::unsupported()
    }
    pub fn cancel_trial(&mut self) -> R<()> {
        Self::unsupported()
    }
}

#[cfg(not(windows))]
pub struct Camera {
    #[cfg(target_os = "linux")]
    v4l: std::fs::File,
    #[cfg(not(target_os = "linux"))]
    h: DeviceHandle<GlobalContext>,
    #[cfg(not(target_os = "linux"))]
    vc_if: u16,
    seq: u32,
    status: [u8; FRAME],
}

#[cfg(not(windows))]
impl Camera {
    pub fn present() -> bool {
        crate::usb::present(&CAMERA_IDS)
    }

    #[cfg(target_os = "linux")]
    pub fn open() -> R<Self> {
        Ok(Camera { v4l: v4l::find()?, seq: 0, status: [0; FRAME] })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open() -> R<Self> {
        let dev = rusb::devices()
            .map_err(|e| e.to_string())?
            .iter()
            .find(|d| {
                d.device_descriptor().map(|x| camera_id(x.vendor_id(), x.product_id())).unwrap_or(false)
            })
            .ok_or("k1cam camera (f055:4b31 or legacy 1d6b:0110) not found; is it running the k1cam firmware?")?;
        let cfg = dev.active_config_descriptor().map_err(|e| e.to_string())?;
        let vc_if = cfg
            .interfaces()
            .flat_map(|i| i.descriptors())
            .find(|d| d.class_code() == 14 && d.sub_class_code() == 1)
            .map(|d| d.interface_number() as u16)
            .ok_or("no UVC control interface")?;
        let h = dev.open().map_err(|e| format!("open camera: {e}"))?;
        let c = Camera { h, vc_if, seq: 0, status: [0; FRAME] };
        let mut len = [0u8; 2];
        c.h.read_control(0xa1, 0x85, SEL_COMMAND << 8, c.index(), &mut len, Duration::from_secs(2))
            .map_err(|e| format!("firmware protocol not available: {e}"))?;
        if u16::from_le_bytes(len) as usize != FRAME {
            return Err("camera firmware protocol has an unexpected frame size".into());
        }
        Ok(c)
    }

    #[cfg(not(target_os = "linux"))]
    fn index(&self) -> u16 {
        (UNIT << 8) | self.vc_if
    }

    #[cfg(target_os = "linux")]
    fn set(&self, sel: u16, f: &[u8; FRAME]) -> std::io::Result<()> {
        let mut b = *f;
        v4l::query(&self.v4l, UNIT as u8, sel as u8, 0x01, &mut b)
    }

    #[cfg(target_os = "linux")]
    fn get(&self, sel: u16) -> std::io::Result<[u8; FRAME]> {
        let mut b = [0u8; FRAME];
        v4l::query(&self.v4l, UNIT as u8, sel as u8, 0x81, &mut b)?;
        Ok(b)
    }

    #[cfg(not(target_os = "linux"))]
    fn set(&self, sel: u16, f: &[u8; FRAME]) -> rusb::Result<()> {
        self.h.write_control(0x21, 0x01, sel << 8, self.index(), f, Duration::from_secs(2)).map(|_| ())
    }

    #[cfg(not(target_os = "linux"))]
    fn get(&self, sel: u16) -> rusb::Result<[u8; FRAME]> {
        let mut b = [0u8; FRAME];
        self.h.read_control(0xa1, 0x81, sel << 8, self.index(), &mut b, Duration::from_secs(2))?;
        Ok(b)
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }

    fn poll_status(&mut self) -> R<()> {
        let s = self.get(SEL_STATUS).map_err(|e| format!("status: {e}"))?;
        if &s[..4] != b"K1FW" {
            return Err("bad status frame".into());
        }
        self.status = s;
        Ok(())
    }

    fn result(&self) -> u8 {
        self.status[7]
    }

    fn detail(&self) -> u32 {
        le32(&self.status[36..])
    }

    fn phase(&self) -> u8 {
        self.status[5]
    }

    fn session(&self) -> u32 {
        le32(&self.status[12..])
    }

    /// Sends one frame and waits for its STATUS; returns the result code.
    fn send(&mut self, sel: u16, f: &[u8; FRAME], seq: u32) -> R<u8> {
        for _ in 0..4 {
            let _ = self.set(sel, f);
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if self.poll_status().is_ok() {
                    if le32(&self.status[8..]) == seq {
                        return Ok(self.result());
                    }
                    break; // older status: the frame did not arrive, resend
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Err("camera did not answer".into())
    }

    fn command(&mut self, op: u8, target: u8, flags: u8, session: u32, offset: u32, length: u32) -> R<u8> {
        let seq = self.next_seq();
        let f = command_frame(op, target, flags, seq, session, offset, length);
        self.send(SEL_COMMAND, &f, seq)
    }

    fn command_ok(&mut self, what: &str, op: u8, flags: u8, session: u32, length: u32) -> R<()> {
        match self.command(op, 0, flags, session, 0, length)? {
            0 => Ok(()),
            e => Err(format!("{what}: {}", error_text(e, self.detail()))),
        }
    }

    fn window(&mut self, target: u8, session: u32, offset: u32) -> R<Vec<u8>> {
        for _ in 0..4 {
            let seq = self.next_seq();
            let f = command_frame(OP_READ, target, 0, seq, session, offset, 0);
            let _ = self.set(SEL_COMMAND, &f);
            if let Ok(w) = self.get(SEL_DATA) {
                let n = w[12] as usize;
                if n <= DATA_MAX {
                    let crc = crc16(crc16(0xffff, &w[..14]), &w[16..16 + n]);
                    if crc == u16::from_le_bytes([w[14], w[15]])
                        && le32(&w[8..]) == seq
                        && le32(&w[0..]) == session
                        && le32(&w[4..]) == offset
                        && w[13] == target
                    {
                        return Ok(w[16..16 + n].to_vec());
                    }
                }
            }
            if self.poll_status().is_ok() && le32(&self.status[8..]) == seq && self.result() != 0 {
                return Err(format!("read: {}", error_text(self.result(), self.detail())));
            }
        }
        Err(format!("read at {offset}: no valid reply"))
    }

    fn object(&mut self, target: u8, session: u32) -> R<String> {
        let mut out = Vec::new();
        loop {
            let w = self.window(target, session, out.len() as u32)?;
            out.extend_from_slice(&w);
            if w.len() < DATA_MAX || out.len() > 4096 {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub fn info(&mut self) -> R<String> {
        self.command_ok("info", OP_INFO, 0, 0, 0)?;
        self.object(TARGET_INFO, 0)
    }

    fn wait_while(&mut self, job: &Job, busy: &[u8], secs: u64) -> R<()> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            job.check_cancel()?;
            self.poll_status()?;
            if !busy.contains(&self.phase()) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err("timed out".into())
    }

    pub fn dump(&mut self, job: &Job, live: bool) -> R<(Vec<u8>, String)> {
        self.command_ok("begin dump", OP_BEGIN_DUMP, live as u8, 0, 0)?;
        let session = self.session();
        let r = self.dump_session(job, session);
        if r.is_err() {
            let _ = self.command_ok("abort", OP_ABORT, 0, session, 0);
        }
        r
    }

    fn dump_session(&mut self, job: &Job, session: u32) -> R<(Vec<u8>, String)> {
        job.log("Camera is preparing a flash snapshot");
        self.wait_while(job, &[PH_ACCEPTED, PH_READING], 120)?;
        if self.phase() == PH_ERROR {
            return Err(format!("dump: {}", error_text(self.status[35], self.detail())));
        }
        if self.phase() != PH_READY {
            return Err(format!("dump: snapshot not ready ({})", phase_name(self.phase())));
        }
        let manifest = self.object(TARGET_MANIFEST, session)?;
        let size: u32 = record_value(&manifest, "size").and_then(|s| s.parse().ok()).unwrap_or(0);
        if size != FW_FLASH_SIZE {
            return Err(format!("unexpected flash size {size}"));
        }
        let consistency = record_value(&manifest, "consistency").unwrap_or("").to_string();
        let mut out = Vec::with_capacity(size as usize);
        let mut sha = Sha256::new();
        while (out.len() as u32) < size {
            job.check_cancel()?;
            let w = self.window(TARGET_IMAGE, session, out.len() as u32)?;
            if w.is_empty() {
                return Err(format!("dump ended early at {}", out.len()));
            }
            sha.update(&w);
            out.extend_from_slice(&w);
            if out.len() % (DATA_MAX * 256) < DATA_MAX {
                job.progress(out.len() as f32 / size as f32);
            }
        }
        let _ = self.command_ok("end dump", OP_END_DUMP, 0, session, 0);
        let hex = hex(&sha.finalize());
        if let Some(want) = record_value(&manifest, "sha256") {
            if want != hex {
                return Err("received data does not match the camera's digest".into());
            }
        }
        Ok((out, consistency))
    }

    pub fn upgrade(&mut self, job: &Job, b: &Bundle, reboot: bool) -> R<String> {
        let info = self.info()?;
        let get = |k| record_value(&info, k).unwrap_or("");
        let target = install_slot(&info, b)?;
        let serial = get("serial").to_string();
        job.log(format!("Installing {} into slot {target} (camera runs {})", b.build, get("build")));

        let seq = self.next_seq();
        let mut f = command_frame(OP_BEGIN_UPGRADE, 0, 0, seq, 0, 0, b.data.len() as u32);
        f[24..56].copy_from_slice(&b.sha256);
        match self.send(SEL_COMMAND, &f, seq)? {
            0 => {}
            e => return Err(format!("begin upgrade: {}", error_text(e, self.detail()))),
        }
        let session = self.session();
        let r = self.upgrade_session(job, b, session);
        if r.is_err() {
            let _ = self.command_ok("abort", OP_ABORT, 0, session, 0);
            return r.map(|_| String::new());
        }
        let mut msg = format!("Staged {} in slot {target} as a trial.", b.build);
        if reboot {
            self.command_ok("reboot", OP_REBOOT, 0, session, 0)?;
            job.log("Rebooting; waiting for the camera to come back");
            msg.push_str(&Self::wait_reboot(job, &serial, &b.build));
        } else {
            msg.push_str(" Reboot the camera to start the trial.");
        }
        Ok(msg)
    }

    fn upgrade_session(&mut self, job: &Job, b: &Bundle, session: u32) -> R<()> {
        job.log("Uploading bundle");
        let len = b.data.len();
        let mut off = 0;
        while off < len {
            job.check_cancel()?;
            let n = DATA_MAX.min(len - off);
            let seq = self.next_seq();
            let f = data_frame(session, off as u32, seq, &b.data[off..off + n]);
            match self.send(SEL_DATA, &f, seq)? {
                0 => {}
                e => return Err(format!("upload: {}", error_text(e, self.detail()))),
            }
            off += n;
            if off % (DATA_MAX * 256) < DATA_MAX {
                job.progress(off as f32 / len as f32);
            }
        }
        job.progress(1.0);
        self.command_ok("finish upgrade", OP_FINISH_UPGRADE, 0, session, len as u32)?;
        job.log("Camera is validating and programming the slot");
        self.wait_while(job, &[PH_ACCEPTED, PH_VALIDATING, PH_ERASING, PH_PROGRAMMING, PH_VERIFYING], 600)?;
        match self.phase() {
            PH_STAGED => Ok(()),
            PH_ERROR => Err(format!("install: {}", error_text(self.status[35], self.detail()))),
            p => Err(format!("install ended {}", phase_name(p))),
        }
    }

    fn wait_reboot(job: &Job, serial: &str, build: &str) -> String {
        let end = Instant::now() + Duration::from_secs(180);
        std::thread::sleep(Duration::from_secs(5));
        while Instant::now() < end {
            if job.check_cancel().is_err() {
                break;
            }
            if let Ok(mut c) = Camera::open() {
                if let Ok(info) = c.info() {
                    let same = record_value(&info, "serial").map_or(true, |s| serial.is_empty() || s == serial);
                    if same && record_value(&info, "build") == Some(build) {
                        return format!(" Camera is back running {build}, boot: {}.", record_value(&info, "boot").unwrap_or("?"));
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        " The camera did not report the new build within 3 minutes.".into()
    }

    pub fn reboot(&mut self) -> R<()> {
        self.command_ok("reboot", OP_REBOOT, 0, 0, 0)
    }

    pub fn cancel_trial(&mut self) -> R<()> {
        self.command_ok("cancel", OP_ABORT, 1, 0, 0)
    }
}

/// The slot a camera with this info record would install b into, or why it would refuse.
pub fn install_slot(info: &str, b: &Bundle) -> R<&'static str> {
    let get = |k| record_value(info, k).unwrap_or("");
    if get("layout") != crate::bundle::LAYOUT {
        return Err(format!(
            "the camera's flash layout is {:?}: flash k1cam.bin once with the cloner, then update with bundles",
            get("layout")
        ));
    }
    if get("board") != b.board {
        return Err(format!("this bundle is for board {}, the camera is {}", b.board, get("board")));
    }
    if b.min_boot > get("boot_compat").parse().unwrap_or(0) {
        return Err("this bundle needs a newer bootloader than the camera has".into());
    }
    if !get("features").contains("upgrade") {
        return Err(format!(
            "the camera cannot update now (boot: {}, trial slot: {}); let a pending trial confirm or cancel it first",
            get("boot"),
            get("trial_slot")
        ));
    }
    if get("build") == b.build {
        return Err(format!("the camera already runs {}", b.build));
    }
    Ok(if get("active_slot") == "a" { "b" } else { "a" })
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_check_value() {
        assert_eq!(crc16(0xffff, b"123456789"), 0x29b1);
    }

    #[test]
    fn data_frame_crc_verifies() {
        let f = data_frame(7, 88, 3, b"hello");
        let crc = crc16(crc16(0xffff, &f[..14]), &f[16..21]);
        assert_eq!(crc, u16::from_le_bytes([f[14], f[15]]));
        assert_eq!(f[12], 5);
    }

    #[test]
    fn command_layout() {
        let f = command_frame(OP_READ, 1, 0, 9, 2, 3, 4);
        assert_eq!(&f[..8], b"K1FW\x01\x02\x01\x00");
        assert_eq!(le32(&f[8..]), 9);
        assert_eq!(le32(&f[20..]), 4);
    }
}
