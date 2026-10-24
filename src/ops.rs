// SPDX-License-Identifier: MIT
//! What the GUI and the command line both do: back up and flash the whole NOR
//! (cloner), and dump and update a running camera (A/B).

use crate::bundle::Bundle;
use crate::job::Job;
use crate::{cloner, xu};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const FLASH_SIZE: usize = 8 * 1024 * 1024;

/// The time now in UTC, written as k1cam's build ids write it (20261020T074512Z).
pub fn stamp() -> String {
    stamp_at(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

fn stamp_at(t: u64) -> String {
    // Days to a civil date: Howard Hinnant's civil_from_days.
    let z = (t / 86400) as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + (m <= 2) as i64;
    let s = t % 86400;
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

pub fn backup(j: &Job, path: &Path) -> Result<String, String> {
    let data = cloner::Cloner::read_flash(j)?;
    std::fs::write(path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{} sha256 {}", path.display(), xu::hex(&Sha256::digest(&data))))
}

pub fn flash(j: &Job, image: &Path, backup_first: bool, verify: bool, reboot: bool) -> Result<String, String> {
    let data = std::fs::read(image).map_err(|e| format!("{}: {e}", image.display()))?;
    if data.len() != FLASH_SIZE {
        return Err(format!("{} must be exactly 8 MiB (got {} bytes)", image.display(), data.len()));
    }
    if backup_first {
        let mut name = image.file_stem().unwrap_or_default().to_os_string();
        name.push(format!("-backup-{}.bin", stamp()));
        let path = image.with_file_name(name);
        j.log("Backing up the current flash first");
        j.log(backup(j, &path)?);
    }
    cloner::Cloner::write_flash(j, &data, false)?;
    if verify {
        j.log("Verifying");
        let back = cloner::Cloner::read_flash(j)?;
        if back != data {
            return Err("verification failed: flash contents differ from the image".into());
        }
        j.log("Verified");
    }
    if reboot {
        cloner::Cloner::reboot(j)?;
    }
    Ok("flashed with the cloner".into())
}

pub fn dump(j: &Job, path: &Path, live: bool) -> Result<String, String> {
    let (data, consistency) = xu::Camera::open()?.dump(j, live)?;
    std::fs::write(path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{} ({consistency}), sha256 {}", path.display(), xu::hex(&Sha256::digest(&data))))
}

pub fn update(j: &Job, b: &Bundle, reboot: bool) -> Result<String, String> {
    j.log(format!("Bundle {} ({} bytes)", b.build, b.data.len()));
    xu::Camera::open()?.upgrade(j, b, reboot)
}

#[cfg(test)]
mod tests {
    #[test]
    fn stamps() {
        assert_eq!(super::stamp_at(0), "19700101T000000Z");
        assert_eq!(super::stamp_at(951_782_400), "20000229T000000Z");
        assert_eq!(super::stamp_at(1_792_481_892), "20261020T073812Z");
        assert_eq!(super::stamp_at(4_102_444_799), "20991231T235959Z");
    }
}
