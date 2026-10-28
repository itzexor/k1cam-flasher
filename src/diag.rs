// SPDX-License-Identifier: MIT
//! ISP diagnostic snapshot v1, independent of the firmware exchange.
// The remote API transports these exact bytes; both paths use this parser.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub sequence: u16,
    pub flags: u8,
    pub valid: u16,
    /// Camera clock, preserved for diagnostic tools; GUI ages arrivals locally.
    #[cfg_attr(not(test), allow(dead_code))]
    pub monotonic_ms: u64,
    pub metering: u8,
    pub requested: u8,
    pub applied: u8,
    pub guard_top: u8,
    pub target: u16,
    pub mean: u16,
    pub exposure_us: u32,
    pub gain_x256: u32,
    pub awb_kelvin: u16,
    pub sensor: [u16; 2],
    pub view: [u16; 4],
    pub zoom: u16,
    pub rotation: u16,
    pub guard_level: f32,
    pub guard_cut: f32,
    pub weights: [[u8; 15]; 15],
    pub zones: [[u16; 15]; 15],
    pub hist: [u16; 64],
}

pub const SIZE: usize = 745;
const PAYLOAD: usize = 56;
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
impl Snapshot {
    pub fn has(&self, bits: u16) -> bool {
        self.valid & bits == bits
    }

    pub fn parse(r: &[u8]) -> Result<Self, String> {
        if r.len() < SIZE || r[0] != 1 {
            return Err("unsupported diagnostic record".into());
        }
        let u16at = |i| u16::from_le_bytes([r[i], r[i + 1]]);
        let u32at = |i| u32::from_le_bytes(r[i..i + 4].try_into().unwrap());
        if usize::from(u16at(4)) != r.len() {
            return Err("invalid diagnostic record length".into());
        }
        let sensor = [u16at(34), u16at(36)];
        let view = [u16at(38), u16at(40), u16at(42), u16at(44)];
        let valid = u16at(6);
        if valid & 8 != 0
            && (sensor.contains(&0)
                || view[2..].contains(&0)
                || u32::from(view[0]) + u32::from(view[2]) > u32::from(sensor[0])
                || u32::from(view[1]) + u32::from(view[3]) > u32::from(sensor[1]))
        {
            return Err("invalid diagnostic view".into());
        }
        let mut weights = [[0; 15]; 15];
        let mut zones = [[0; 15]; 15];
        for i in 0..225 {
            weights[i / 15][i % 15] = (r[54 + i / 2] >> (4 * (i % 2))) & 15;
            zones[i / 15][i % 15] = u16at(167 + i * 2);
        }
        if valid & 16 != 0 && weights.iter().flatten().any(|&w| w > 8) {
            return Err("invalid diagnostic weight".into());
        }
        Ok(Self {
            sequence: u16at(2),
            flags: r[1],
            valid,
            monotonic_ms: u64::from_le_bytes(r[8..16].try_into().unwrap()),
            metering: r[16],
            requested: r[17],
            applied: r[18],
            guard_top: r[19],
            target: u16at(20),
            mean: u16at(22),
            exposure_us: u32at(24),
            gain_x256: u32at(28),
            awb_kelvin: u16at(32),
            sensor,
            view,
            zoom: u16at(46),
            rotation: u16at(48),
            guard_level: f32::from(u16at(50)) / 100.0,
            guard_cut: f32::from(u16at(52)) / 10000.0,
            weights,
            zones,
            hist: std::array::from_fn(|i| u16at(617 + i * 2)),
        })
    }
}

/// Latch and collect once. A competing reader is detected, never silently mixed.
/// None means older firmware; transport failures remain errors.
#[cfg(any(target_os = "linux", test))]
pub fn read(mut query: impl FnMut(u8, u8, &mut [u8]) -> std::io::Result<()>) -> Result<Option<Vec<u8>>, String> {
    let mut len = [0; 2];
    match query(30, 0x85, &mut len) {
        Err(e) if matches!(e.raw_os_error(), Some(2) | Some(22)) => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(()) => {}
    }
    if u16::from_le_bytes(len) != 60 {
        return Ok(None);
    }
    let mut version = [0];
    query(29, 0x81, &mut version).map_err(|e| e.to_string())?;
    if version[0] != 1 {
        return Ok(None);
    }
    for _ in 0..2 {
        query(29, 0x01, &mut [0]).map_err(|e| e.to_string())?;
        let mut record = Vec::new();
        let (mut count, mut seq) = (1, [0; 2]);
        let mut i = 0;
        while i < count {
            let mut page = [0; 60];
            query(30, 0x81, &mut page).map_err(|e| e.to_string())?;
            if i == 0 {
                count = usize::from(page[1]);
                seq.copy_from_slice(&page[2..4]);
            }
            if !(SIZE.div_ceil(PAYLOAD)..=32).contains(&count) || usize::from(page[0]) != i || usize::from(page[1]) != count || page[2..4] != seq {
                break;
            }
            record.extend_from_slice(&page[4..]);
            i += 1;
        }
        if i != count || record.len() < SIZE || record[2..4] != seq {
            continue;
        }
        let size = usize::from(u16::from_le_bytes([record[4], record[5]]));
        if size < SIZE || size <= (count - 1) * PAYLOAD || size > record.len() {
            continue;
        }
        record.truncate(size);
        Snapshot::parse(&record)?;
        return Ok(Some(record));
    }
    Err("diagnostic snapshot changed while reading its pages".into())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    pub fn record() -> Vec<u8> {
        let mut r = vec![0; SIZE];
        r[0] = 1;
        r[1] = 15;
        r[2..4].copy_from_slice(&42u16.to_le_bytes());
        r[4..6].copy_from_slice(&(SIZE as u16).to_le_bytes());
        r[6..8].copy_from_slice(&511u16.to_le_bytes());
        r[8..16].copy_from_slice(&123456u64.to_le_bytes());
        r[16] = 4;
        r[17] = 200;
        r[18] = 150;
        r[19] = 220;
        r[24..28].copy_from_slice(&12345u32.to_le_bytes());
        r[28..32].copy_from_slice(&256u32.to_le_bytes());
        for (offset, value) in [(34, 1920u16), (36, 1080), (38, 480), (40, 270), (42, 960), (44, 540), (46, 200), (48, 180), (50, 17025), (52, 2500)] {
            r[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        for i in 0..225 {
            r[54 + i / 2] |= ((i % 9) as u8) << (4 * (i % 2));
            r[167 + i * 2..169 + i * 2].copy_from_slice(&(i as u16).to_le_bytes());
        }
        r[617..619].copy_from_slice(&65535u16.to_le_bytes());
        r
    }

    #[test]
    fn parses_layout_and_rejects_invalid_records() {
        let r = record();
        let d = Snapshot::parse(&r).unwrap();
        assert_eq!((d.sequence, d.monotonic_ms, d.exposure_us, d.gain_x256), (42, 123456, 12345, 256));
        assert_eq!((d.view, d.rotation, d.guard_level, d.guard_cut), ([480, 270, 960, 540], 180, 170.25, 0.25));
        assert_eq!((d.weights[1][0], d.zones[1][0], d.zones[14][14], d.hist[0]), (6, 15, 224, 65535));
        for n in 0..SIZE {
            assert!(Snapshot::parse(&r[..n]).is_err());
        }
        let mut bad = r.clone();
        bad[0] = 2;
        assert!(Snapshot::parse(&bad).is_err());
        let mut bad = r.clone();
        bad[42] = 0;
        bad[43] = 0;
        assert!(Snapshot::parse(&bad).is_err());
        let mut bad = r.clone();
        bad[54] = 15;
        assert!(Snapshot::parse(&bad).is_err());
        let mut extended = r.clone();
        extended.extend_from_slice(&[1, 2, 3]);
        let n = extended.len() as u16;
        extended[4..6].copy_from_slice(&n.to_le_bytes());
        assert!(Snapshot::parse(&extended).is_ok());
    }

    fn collect(mix: bool) -> Result<Option<Vec<u8>>, String> {
        let r = record();
        let mut page = 0;
        read(|s, q, out| {
            match (s, q) {
                (30, 0x85) => out.copy_from_slice(&60u16.to_le_bytes()),
                (29, 0x81) => out[0] = 1,
                (29, 0x01) => page = 0,
                (30, 0x81) => {
                    out[0] = page as u8;
                    out[1] = SIZE.div_ceil(PAYLOAD) as u8;
                    out[2..4].copy_from_slice(&(if mix && page == 2 { 43u16 } else { 42u16 }).to_le_bytes());
                    let start = page * PAYLOAD;
                    let end = (start + PAYLOAD).min(r.len());
                    out[4..4 + end - start].copy_from_slice(&r[start..end]);
                    page += 1;
                }
                _ => panic!("unexpected request"),
            }
            Ok(())
        })
    }

    #[test]
    fn collects_pages_and_refuses_mixed_snapshots() {
        assert_eq!(collect(false).unwrap().unwrap(), record());
        assert!(collect(true).unwrap_err().contains("changed while reading"));
        for count in [0, 1, 13, 33, 255] {
            assert!(read(|s, q, out| {
                if (s, q) == (30, 0x85) {
                    out.copy_from_slice(&60u16.to_le_bytes());
                } else if (s, q) == (29, 0x81) {
                    out[0] = 1;
                } else if (s, q) == (30, 0x81) {
                    out[1] = count;
                }
                Ok(())
            })
            .is_err());
        }
        assert!(read(|_, _, _| Err(std::io::Error::from_raw_os_error(2))).unwrap().is_none());
        assert!(read(|_, _, _| Err(std::io::Error::from_raw_os_error(5))).is_err());
    }
}

/// Snapshot headers use hex to keep the JPEG body and old clients unchanged.
#[cfg(feature = "gui")]
pub fn from_hex(s: &str) -> Result<Snapshot, String> {
    if s.len() % 2 != 0 || s.len() > 32 * 56 * 2 {
        return Err("invalid snapshot diagnostics".into());
    }
    let bytes: Option<Vec<u8>> = s.as_bytes().chunks_exact(2).map(|p| std::str::from_utf8(p).ok().and_then(|p| u8::from_str_radix(p, 16).ok())).collect();
    Snapshot::parse(&bytes.ok_or("invalid snapshot diagnostics")?)
}
