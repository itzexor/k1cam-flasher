// SPDX-License-Identifier: MIT
//! The camera's picture over V4L2 (Linux): MJPEG frames by mmap streaming, and
//! its image controls, the standard ones by VIDIOC_*_CTRL and k1cam's own on
//! Extension Unit 4 by uvcvideo's XU query (as uvcdctl does).

use crate::controls::{Kind, Value};
use crate::linux::{ioc, ioctl, IOC_READ, IOC_WRITE};
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

type R<T> = Result<T, String>;

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const BUF_FLAG_ERROR: u32 = 0x40;
const CTRL_FLAG_INACTIVE: u32 = 0x10;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const MJPEG: u32 = u32::from_le_bytes(*b"MJPG");

// linux/videodev2.h
#[repr(C)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    ty: u32,
    // discrete: width, height; stepwise: six values
    size: [u32; 6],
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
union FormatUnion {
    pix: PixFormat,
    raw: [u8; 200],
    // struct v4l2_window holds pointers, which sets the union's alignment.
    _align: [*mut libc::c_void; 0],
}

#[repr(C)]
struct Format {
    ty: u32,
    fmt: FormatUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[repr(C)]
union ParmUnion {
    capture: CaptureParm,
    raw: [u8; 200],
}

#[repr(C)]
struct StreamParm {
    ty: u32,
    parm: ParmUnion,
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    ty: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
struct Timecode {
    ty: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
union BufferM {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut libc::c_void,
    fd: i32,
}

#[repr(C)]
struct Buffer {
    index: u32,
    ty: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    /// The kernel's own timeval, two longs: a 32-bit libc with 64-bit time
    /// would change the struct's size, and with it the ioctl numbers, to ones
    /// that only kernels from 5.6 on know.
    timestamp: [libc::c_long; 2],
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: BufferM,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

#[repr(C)]
struct QueryCtrl {
    id: u32,
    ty: u32,
    name: [u8; 32],
    minimum: i32,
    maximum: i32,
    step: i32,
    default_value: i32,
    flags: u32,
    reserved: [u32; 2],
}

#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

const VIDIOC_S_FMT: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 5, size_of::<Format>());
const VIDIOC_REQBUFS: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 9, size_of::<Buffer>());
const VIDIOC_QBUF: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 15, size_of::<Buffer>());
const VIDIOC_DQBUF: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 17, size_of::<Buffer>());
const VIDIOC_STREAMON: u32 = ioc(IOC_WRITE, b'V', 18, size_of::<libc::c_int>());
const VIDIOC_STREAMOFF: u32 = ioc(IOC_WRITE, b'V', 19, size_of::<libc::c_int>());
const VIDIOC_S_PARM: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 22, size_of::<StreamParm>());
const VIDIOC_G_CTRL: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 27, size_of::<Control>());
const VIDIOC_S_CTRL: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 28, size_of::<Control>());
const VIDIOC_QUERYCTRL: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 36, size_of::<QueryCtrl>());
const VIDIOC_ENUM_FRAMESIZES: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 74, size_of::<FrmSizeEnum>());

// UVC requests for the Extension Unit (linux/usb/video.h).
const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const UVC_GET_MIN: u8 = 0x82;
const UVC_GET_MAX: u8 = 0x83;
const UVC_GET_RES: u8 = 0x84;
const UVC_GET_DEF: u8 = 0x87;

/// The attached camera's capture node.
pub fn node() -> Option<PathBuf> {
    crate::linux::video_node(&crate::xu::CAMERA_IDS)
}

fn open(node: &Path) -> R<File> {
    std::fs::OpenOptions::new().read(true).write(true).open(node).map_err(|e| format!("{}: {e}", node.display()))
}

/// What a failed open or ioctl means here, with the programs that hold the
/// node when it is busy.
fn busy_or(node: &Path, what: &str, e: io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EBUSY) => {
            let users = crate::linux::device_users(&node.to_string_lossy());
            if users.is_empty() {
                "another program is using the camera's video".into()
            } else {
                format!("the camera's video is in use by {}", users.join(", "))
            }
        }
        Some(libc::ENODEV) => "the camera went away".into(),
        _ => format!("{what}: {e}"),
    }
}

/// The MJPEG frame sizes the camera offers, largest first.
pub fn frame_sizes(node: &Path) -> R<Vec<(u32, u32)>> {
    let f = open(node)?;
    let mut sizes = Vec::new();
    for index in 0..64 {
        let mut e = FrmSizeEnum { index, pixel_format: MJPEG, ty: 0, size: [0; 6], reserved: [0; 2] };
        if ioctl(&f, VIDIOC_ENUM_FRAMESIZES, &mut e).is_err() {
            break;
        }
        if e.ty == FRMSIZE_TYPE_DISCRETE {
            sizes.push((e.size[0], e.size[1]));
        }
    }
    if sizes.is_empty() {
        return Err("the camera offers no MJPEG video".into());
    }
    sizes.sort_by_key(|&(w, h)| std::cmp::Reverse(w * h));
    Ok(sizes)
}

/// An MJPEG capture: buffers mapped from the driver, queued while streaming.
pub struct Stream {
    f: File,
    node: PathBuf,
    bufs: Vec<(*mut libc::c_void, usize)>,
    pub width: u32,
    pub height: u32,
}

// The mappings belong to the stream alone; it moves to its capture thread.
unsafe impl Send for Stream {}

impl Stream {
    pub fn start(node: &Path, width: u32, height: u32, fps: u32) -> R<Self> {
        let f = open(node)?;
        let mut s = Self { f, node: node.into(), bufs: Vec::new(), width, height };
        let mut fmt: Format = unsafe { zeroed() };
        fmt.ty = BUF_TYPE_VIDEO_CAPTURE;
        fmt.fmt.pix = PixFormat { width, height, pixelformat: MJPEG, ..unsafe { zeroed() } };
        ioctl(&s.f, VIDIOC_S_FMT, &mut fmt).map_err(|e| busy_or(node, "set format", e))?;
        let pix = unsafe { fmt.fmt.pix };
        if pix.pixelformat != MJPEG {
            return Err("the camera would not send MJPEG".into());
        }
        (s.width, s.height) = (pix.width, pix.height);
        let mut parm: StreamParm = unsafe { zeroed() };
        parm.ty = BUF_TYPE_VIDEO_CAPTURE;
        parm.parm.capture.numerator = 1;
        parm.parm.capture.denominator = fps;
        // The camera keeps its default rate if it doesn't take this one.
        let _ = ioctl(&s.f, VIDIOC_S_PARM, &mut parm);
        let mut req =
            RequestBuffers { count: 4, ty: BUF_TYPE_VIDEO_CAPTURE, memory: MEMORY_MMAP, capabilities: 0, flags: 0, reserved: [0; 3] };
        ioctl(&s.f, VIDIOC_REQBUFS, &mut req).map_err(|e| busy_or(node, "request buffers", e))?;
        for index in 0..req.count {
            let mut b = Self::buffer(index);
            ioctl(&s.f, VIDIOC_QUERYBUF, &mut b).map_err(|e| format!("query buffer: {e}"))?;
            let len = b.length as usize;
            let off = unsafe { b.m.offset } as libc::off_t;
            let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, s.f.as_raw_fd(), off) };
            if p == libc::MAP_FAILED {
                return Err(format!("map buffer: {}", io::Error::last_os_error()));
            }
            s.bufs.push((p, len));
            ioctl(&s.f, VIDIOC_QBUF, &mut b).map_err(|e| format!("queue buffer: {e}"))?;
        }
        let mut ty = BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        ioctl(&s.f, VIDIOC_STREAMON, &mut ty).map_err(|e| busy_or(node, "start streaming", e))?;
        Ok(s)
    }

    fn buffer(index: u32) -> Buffer {
        let mut b: Buffer = unsafe { zeroed() };
        b.index = index;
        b.ty = BUF_TYPE_VIDEO_CAPTURE;
        b.memory = MEMORY_MMAP;
        b
    }

    /// The next frame, or None if none came within `timeout` (or it arrived damaged).
    pub fn frame(&mut self, timeout: Duration) -> R<Option<Vec<u8>>> {
        let mut pfd = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(format!("poll: {e}")) };
        }
        if n == 0 {
            return Ok(None);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 && pfd.revents & libc::POLLIN == 0 {
            return Err(busy_or(&self.node, "stream", io::Error::from_raw_os_error(libc::ENODEV)));
        }
        let mut b = Self::buffer(0);
        ioctl(&self.f, VIDIOC_DQBUF, &mut b).map_err(|e| busy_or(&self.node, "dequeue", e))?;
        let frame = self.bufs.get(b.index as usize).filter(|_| b.flags & BUF_FLAG_ERROR == 0).map(|&(p, len)| {
            let n = (b.bytesused as usize).min(len);
            unsafe { std::slice::from_raw_parts(p as *const u8, n) }.to_vec()
        });
        ioctl(&self.f, VIDIOC_QBUF, &mut b).map_err(|e| busy_or(&self.node, "queue buffer", e))?;
        Ok(frame.filter(|f| !f.is_empty()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let mut ty = BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        let _ = ioctl(&self.f, VIDIOC_STREAMOFF, &mut ty);
        for &(p, len) in &self.bufs {
            unsafe { libc::munmap(p, len) };
        }
    }
}

/// The camera's image controls, on their own open node.
pub struct Controls {
    f: File,
}

/// Whether an error means the camera lacks the control (older firmware), as
/// opposed to failing to answer.
pub fn unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::EINVAL))
}

impl Controls {
    pub fn open(node: &Path) -> R<Self> {
        Ok(Self { f: open(node)? })
    }

    fn xu(&self, selector: u8, query: u8, value: i16) -> io::Result<i16> {
        let mut b = value.to_le_bytes();
        crate::xu::xu_query(&self.f, selector, query, &mut b)?;
        Ok(i16::from_le_bytes(b))
    }

    pub fn read(&self, kind: Kind) -> io::Result<Value> {
        match kind {
            Kind::Std(id) => {
                let mut q: QueryCtrl = unsafe { zeroed() };
                q.id = id;
                ioctl(&self.f, VIDIOC_QUERYCTRL, &mut q)?;
                let mut c = Control { id, value: 0 };
                ioctl(&self.f, VIDIOC_G_CTRL, &mut c)?;
                Ok(Value {
                    value: c.value,
                    min: q.minimum,
                    max: q.maximum,
                    step: q.step.max(1),
                    default: q.default_value,
                    inactive: q.flags & CTRL_FLAG_INACTIVE != 0,
                })
            }
            Kind::Xu(sel) => {
                let get = |query| self.xu(sel, query, 0).map(i32::from);
                Ok(Value {
                    value: get(UVC_GET_CUR)?,
                    min: get(UVC_GET_MIN)?,
                    max: get(UVC_GET_MAX)?,
                    step: get(UVC_GET_RES).unwrap_or(1).max(1),
                    default: get(UVC_GET_DEF)?,
                    inactive: false,
                })
            }
        }
    }

    /// The current value alone.
    pub fn get(&self, kind: Kind) -> io::Result<i32> {
        match kind {
            Kind::Std(id) => {
                let mut c = Control { id, value: 0 };
                ioctl(&self.f, VIDIOC_G_CTRL, &mut c)?;
                Ok(c.value)
            }
            Kind::Xu(sel) => self.xu(sel, UVC_GET_CUR, 0).map(i32::from),
        }
    }

    pub fn write(&self, kind: Kind, value: i32) -> io::Result<()> {
        match kind {
            Kind::Std(id) => ioctl(&self.f, VIDIOC_S_CTRL, &mut Control { id, value }).map(|_| ()),
            Kind::Xu(sel) => self.xu(sel, UVC_SET_CUR, value as i16).map(|_| ()),
        }
    }

    /// Asks the camera for its factory defaults (Extension Unit selector 10).
    pub fn factory_reset(&self) -> io::Result<()> {
        self.xu(10, UVC_SET_CUR, 1).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The values the kernel headers give.
    #[test]
    #[cfg(target_pointer_width = "32")]
    fn ioctl_numbers_32bit() {
        // MIPS puts the write bit where the others put the read bit.
        let (rw, w) = (0xc000_0000u32, if cfg!(target_arch = "mips") { 0x8000_0000 } else { 0x4000_0000 });
        assert_eq!(VIDIOC_S_FMT, rw | 0x00cc_5605);
        assert_eq!(VIDIOC_QUERYBUF, rw | 0x0044_5609);
        assert_eq!(VIDIOC_QBUF, rw | 0x0044_560f);
        assert_eq!(VIDIOC_DQBUF, rw | 0x0044_5611);
        assert_eq!(VIDIOC_STREAMON, w | 0x0004_5612);
        assert_eq!(VIDIOC_S_PARM, rw | 0x00cc_5616);
        assert_eq!(VIDIOC_QUERYCTRL, rw | 0x0044_5624);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn ioctl_numbers_x86_64() {
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        assert_eq!(VIDIOC_QUERYCTRL, 0xc044_5624);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
    }
}
