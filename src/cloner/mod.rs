// SPDX-License-Identifier: GPL-2.0-only
//! Ingenic T31 cloner protocol (USB bootrom -> burner -> SPI NOR), ported from
//! thingino-dfu's libtdfu. Only the T31 family with DDR2 (X/N/L/ZL/ZC/LC) is
//! supported; the K1 camera is a T31L.

use crate::job::Job;
use crate::usb::Device;
use std::time::{Duration, Instant};

mod chips;

const VID: u16 = 0xa108;
const PID: u16 = 0xc309;
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x81;
const REQ_OUT: u8 = 0x40;
const REQ_IN: u8 = 0xc0;

const VR_GET_CPU_INFO: u8 = 0x00;
const VR_SET_DATA_ADDR: u8 = 0x01;
const VR_SET_DATA_LEN: u8 = 0x02;
const VR_FLUSH_CACHE: u8 = 0x03;
const VR_PROG_STAGE1: u8 = 0x04;
const VR_PROG_STAGE2: u8 = 0x05;
const VR_FW_READ: u8 = 0x10;
const VR_FW_HANDSHAKE: u8 = 0x11;
const VR_WRITE: u8 = 0x12;
const VR_READ: u8 = 0x13;
const VR_FW_WRITE2: u8 = 0x14;
const VR_REBOOT: u8 = 0x16;
const VR_READ_STATUS2: u8 = 0x19;
const VR_READ_STATUS4: u8 = 0x26;

const SPL: &[u8] = include_bytes!("data/spl.bin");
const UBOOT: &[u8] = include_bytes!("data/uboot.bin");
const DDR: &[u8] = include_bytes!("data/ddr_t31x.bin");
const DETECT: &[u8] = include_bytes!("data/detect.bin");
const MARKER: &[u8] = include_bytes!("data/marker.bin");

const WRITE_CHUNK: usize = 128 * 1024;
const READ_CHUNK: usize = 1024 * 1024;
const DESC_ERASE_FLAG: usize = 0xc8 + 0x14;

type R<T> = Result<T, String>;

fn ms(n: u64) {
    std::thread::sleep(Duration::from_millis(n));
}

fn usb<T>(what: &str, r: R<T>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

pub fn device_present() -> bool {
    crate::usb::present(&[(VID, PID)])
}

pub struct Cloner<'a> {
    h: Device,
    job: &'a Job,
}

#[derive(Debug, Clone)]
pub struct ChipInfo {
    pub name: String,
    pub jedec: u32,
    pub size: usize,
}

impl<'a> Cloner<'a> {
    pub fn open(job: &'a Job) -> R<Self> {
        let h = usb("open a108:c309 (is the driver/udev rule installed? see README)", Device::open(VID, PID))?;
        usb("claim interface 0", h.claim(0))?;
        Ok(Self { h, job })
    }

    /// Opens the device, retrying while it re-enumerates.
    fn reopen(job: &'a Job, secs: u64) -> R<Self> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            match Self::open(job) {
                Ok(c) => return Ok(c),
                Err(e) if Instant::now() > end => return Err(e),
                Err(_) => ms(250),
            }
        }
    }

    fn vout(&self, req: u8, value: u16, index: u16, data: &[u8]) -> R<()> {
        usb(
            &format!("vendor request 0x{req:02x}"),
            self.h.control_out(REQ_OUT, req, value, index, data, Duration::from_secs(5)),
        )?;
        Ok(())
    }

    fn vin(&self, req: u8, buf: &mut [u8], timeout: u64) -> R<usize> {
        usb(
            &format!("vendor request 0x{req:02x}"),
            self.h.control_in(REQ_IN, req, 0, 0, buf, Duration::from_millis(timeout)),
        )
    }

    fn addr_req(&self, req: u8, v: u32) -> R<()> {
        self.vout(req, (v >> 16) as u16, v as u16, &[])?;
        ms(100);
        Ok(())
    }

    fn bulk_out(&self, data: &[u8], timeout: Duration) -> R<()> {
        let mut off = 0;
        while off < data.len() {
            let n = usb("bulk write", self.h.bulk_out(EP_OUT, &data[off..], timeout))?;
            if n == 0 {
                return Err("bulk write: no progress".into());
            }
            off += n;
        }
        Ok(())
    }

    fn bulk_in(&self, len: usize, timeout: Duration) -> R<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut off = 0;
        while off < len {
            let n = usb("bulk read", self.h.bulk_in(EP_IN, &mut out[off..], timeout))?;
            if n == 0 {
                return Err("bulk read: no progress".into());
            }
            off += n;
        }
        Ok(out)
    }

    /// Bootrom memory load: address, length (padded to 32), then 64 KiB bulk writes.
    fn load(&self, addr: u32, data: &[u8]) -> R<()> {
        let mut buf = data.to_vec();
        buf.resize(data.len().div_ceil(32) * 32, 0);
        for attempt in 1..=3 {
            let r: R<()> = (|| {
                self.addr_req(VR_SET_DATA_ADDR, addr)?;
                self.addr_req(VR_SET_DATA_LEN, buf.len() as u32)?;
                for c in buf.chunks(64 * 1024) {
                    self.bulk_out(c, Duration::from_secs(6))?;
                }
                Ok(())
            })();
            match r {
                Ok(()) => return Ok(()),
                Err(e) if attempt == 3 => return Err(format!("load to 0x{addr:08x}: {e}")),
                Err(_) => ms(200),
            }
        }
        unreachable!()
    }

    fn cpu_info(&self) -> R<[u8; 8]> {
        let mut b = [0u8; 8];
        let n = self.vin(VR_GET_CPU_INFO, &mut b, 2000)?;
        if n < 8 {
            return Err("short CPU info".into());
        }
        Ok(b)
    }

    fn is_firmware_stage(&self) -> R<bool> {
        let b = self.cpu_info()?;
        Ok(b.starts_with(b"Boot") || b.starts_with(b"BOOT"))
    }

    /// Runs a tiny MIPS program on the bootrom that reads the SoC ID registers.
    fn detect_soc(&self) -> R<&'static str> {
        let mut prog = DETECT.to_vec();
        prog.resize(1024, 0);
        self.addr_req(VR_SET_DATA_ADDR, 0x8000_1000)?;
        self.addr_req(VR_SET_DATA_LEN, prog.len() as u32)?;
        self.bulk_out(&prog, Duration::from_secs(5))?;
        self.addr_req(VR_SET_DATA_LEN, 0x7000)?;
        self.addr_req(VR_PROG_STAGE1, 0x8000_1000)?;
        ms(50);
        self.addr_req(VR_SET_DATA_ADDR, 0x8000_1100)?;
        self.addr_req(VR_SET_DATA_LEN, 24)?;
        let r = self.bulk_in(24, Duration::from_secs(2))?;
        let w = |i: usize| u32::from_le_bytes(r[i..i + 4].try_into().unwrap());
        if w(12) != 0xdead_beef || w(0) == 0 {
            return Err("SoC detection program did not run".into());
        }
        let cpu = (w(0) >> 12) & 0xffff;
        let sub = (w(4) >> 16) & 0xffff;
        if cpu != 0x0031 {
            return Err(format!("this is not a T31 (SoC id 0x{:08x})", w(0)));
        }
        Ok(match sub {
            0x3333 => "T31L",
            0x2222 => "T31X",
            0x1111 => "T31N",
            0x5555 => "T31ZL",
            0xdddd => "T31ZC",
            0xeeee => "T31LC",
            0x4444 => return Err("T31A (DDR3) is not supported by this tool".into()),
            0xcccc => return Err("T31AL is not supported by this tool".into()),
            0x6666 => return Err("T31ZX is not supported by this tool".into()),
            _ => "T31",
        })
    }

    /// Uploads DDR config, SPL and the burner U-Boot so the SoC runs the cloner firmware.
    fn bootstrap(&self) -> R<()> {
        let soc = self.detect_soc()?;
        self.job.log(format!("Boot ROM in USB boot mode, SoC: {soc}"));
        self.job.log("Loading DDR config and SPL");
        self.load(0x8000_1000, DDR)?;
        self.load(0x8000_1800, SPL)?;
        self.addr_req(VR_SET_DATA_LEN, 0x7000)?;
        self.addr_req(VR_PROG_STAGE1, 0x8000_1800)?;
        ms(2000);
        self.job.log("Loading the cloner (burner U-Boot)");
        self.load(0x8010_0000, UBOOT)?;
        ms(500);
        let _ = self.vout(VR_FLUSH_CACHE, 0, 0, &[]);
        ms(100);
        let _ = self.addr_req(VR_PROG_STAGE2, 0x8010_0000);
        ms(1000);
        Ok(())
    }

    fn fw_write2(&self, data: &[u8]) -> R<()> {
        let mut ctl = [0u8; 40];
        ctl[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        self.vout(VR_FW_WRITE2, 0, 0, &ctl)?;
        self.bulk_out(data, Duration::from_secs(6))?;
        ms(100);
        Ok(())
    }

    fn fw_ack(&self) -> R<i32> {
        let mut b = [0u8; 4];
        let n = self.vin(VR_FW_READ, &mut b, 5000)?;
        if n != 4 {
            return Err("short ack".into());
        }
        Ok(i32::from_le_bytes(b))
    }

    fn handshake(&self) -> R<()> {
        let mut last = String::new();
        for _ in 0..5 {
            match self.vout(VR_FW_HANDSHAKE, 0, 0, &[]) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
            ms(100);
        }
        Err(last)
    }

    /// Boots the burner (if still in bootrom) and tells it about the flash chip.
    fn prepare(&mut self, erase: bool) -> R<ChipInfo> {
        if !self.is_firmware_stage()? {
            self.bootstrap()?;
            self.job.log("Waiting for the cloner to start");
            ms(2000);
            // The device may re-enumerate; reopen the handle.
            let job = self.job;
            self.h.release(0);
            let new = Self::reopen(job, 15)?;
            let old = std::mem::replace(self, new);
            drop(old);
            if !self.is_firmware_stage()? {
                return Err("the cloner (burner U-Boot) did not start".into());
            }
        }
        self.fw_write2(MARKER)?;
        let _ = self.fw_ack();
        let mut id = [0u8; 4];
        self.vin(VR_READ_STATUS4, &mut id[..3], 2000)?;
        let a = (id[2] as u32) << 16 | (id[1] as u32) << 8 | id[0] as u32;
        let b = (id[0] as u32) << 16 | (id[1] as u32) << 8 | id[2] as u32;
        let (chip, mut desc) = chips::find(a)
            .or_else(|| chips::find(b))
            .ok_or_else(|| format!("unknown SPI NOR JEDEC id {:02x} {:02x} {:02x}", id[0], id[1], id[2]))?;
        self.job.log(format!("Flash: {} ({} KiB, JEDEC {:06x})", chip.name, chip.size / 1024, chip.jedec));
        if !erase {
            desc[DESC_ERASE_FLAG] = 0;
        }
        self.fw_write2(&desc)?;
        ms(500);
        self.handshake()?;
        Ok(chip)
    }

    /// Detects the flash chip without touching its contents.
    pub fn probe(job: &'a Job) -> R<ChipInfo> {
        let mut c = Self::open(job)?;
        c.prepare(false)
    }

    pub fn read_flash(job: &'a Job) -> R<Vec<u8>> {
        let mut c = Self::open(job)?;
        let chip = c.prepare(false)?;
        let mut out = Vec::with_capacity(chip.size);
        while out.len() < chip.size {
            job.check_cancel()?;
            let n = READ_CHUNK.min(chip.size - out.len());
            let mut req = [0u8; 40];
            req[8..12].copy_from_slice(&(out.len() as u32).to_le_bytes());
            req[16..20].copy_from_slice(&(n as u32).to_le_bytes());
            req[24..28].copy_from_slice(&[0, 0, 6, 0]);
            c.vout(VR_READ, 0, 0, &req)?;
            ms(50);
            let mut st = [0u8; 8];
            let _ = c.vin(VR_READ_STATUS2, &mut st, 2000);
            ms(50);
            out.extend(c.bulk_in(n, Duration::from_secs(10))?);
            let _ = c.fw_ack();
            job.progress(out.len() as f32 / chip.size as f32);
        }
        job.log("Read complete");
        Ok(out)
    }

    pub fn reboot(job: &'a Job) -> R<()> {
        let c = Self::open(job)?;
        let _ = c.vout(VR_FLUSH_CACHE, 0, 0, &[]);
        let _ = c.vout(VR_REBOOT, 0, 0, &[]);
        Ok(())
    }

    pub fn write_flash(job: &'a Job, image: &[u8], reboot: bool) -> R<()> {
        let mut c = Self::open(job)?;
        let chip = c.prepare(true)?;
        if image.len() != chip.size {
            return Err(format!("image is {} bytes but the flash is {}", image.len(), chip.size));
        }
        c.addr_req(VR_SET_DATA_ADDR, 0x8010_0000)?;
        job.log("Erasing flash (this takes a while)");
        c.wait_erase(job)?;
        c.addr_req(VR_SET_DATA_LEN, image.len() as u32)?;
        job.log("Writing");
        let mut off = 0;
        while off < image.len() {
            job.check_cancel()?;
            let n = WRITE_CHUNK.min(image.len() - off);
            c.write_chunk(off, &image[off..off + n])?;
            off += n;
            job.progress(off as f32 / image.len() as f32);
        }
        let _ = c.vout(VR_FLUSH_CACHE, 0, 0, &[]);
        job.log("Write complete");
        if reboot {
            let _ = c.vout(VR_REBOOT, 0, 0, &[]);
            job.log("Rebooting the camera");
        }
        Ok(())
    }

    fn wait_erase(&self, job: &Job) -> R<()> {
        let t0 = Instant::now();
        let (min, max) = (Duration::from_secs(5), Duration::from_secs(60));
        let mut last: Option<u32> = None;
        let mut stable = 0;
        while t0.elapsed() < max {
            job.check_cancel()?;
            let mut b = [0u8; 4];
            if self.vin(VR_READ_STATUS2, &mut b, 5000).is_ok() && t0.elapsed() >= min {
                let s = u32::from_le_bytes(b);
                match last {
                    None => {
                        last = Some(s);
                        stable = 1;
                    }
                    Some(l) if l == s => stable += 1,
                    Some(_) => return Ok(()),
                }
                if stable >= 3 {
                    return Ok(());
                }
            }
            ms(500);
        }
        job.log("Erase status did not settle; continuing");
        Ok(())
    }

    fn write_chunk(&self, off: usize, data: &[u8]) -> R<()> {
        let crc = crc32fast::hash(data);
        let mut hs = [0u8; 40];
        hs[8..12].copy_from_slice(&(off as u32).to_le_bytes());
        hs[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        hs[20..24].copy_from_slice(&crc.to_le_bytes());
        hs[24..28].copy_from_slice(&[0, 0, 6, 0]);
        hs[28..32].copy_from_slice(&(!crc).to_le_bytes());
        hs[32..40].copy_from_slice(&[0x20, 0xfb, 0x00, 0x08, 0xa2, 0x77, 0x00, 0x00]);
        let mut last = 0;
        for _ in 0..3 {
            // The burner may not answer this request; the reference tolerates that.
            let _ = self.h.control_out(REQ_OUT, VR_WRITE, 0, 0, &hs, Duration::from_secs(5));
            ms(10);
            self.bulk_out(data, Duration::from_secs(6))?;
            ms(200);
            let mut sink = [0u8; 512];
            let _ = self.h.bulk_in(EP_IN, &mut sink, Duration::from_millis(5));
            ms(350);
            let t0 = Instant::now();
            loop {
                match self.fw_ack() {
                    Ok(-110) | Err(_) if t0.elapsed() < Duration::from_secs(5) => ms(500),
                    Ok(v) => {
                        last = v;
                        break;
                    }
                    Err(_) => {
                        last = -110;
                        break;
                    }
                }
            }
            if last == 0 {
                return Ok(());
            }
        }
        Err(format!("burner rejected the chunk at 0x{off:06x} (ack {last})"))
    }
}
