use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use trafficview_core::{format::counter_rate, DiskSample, FilesystemSample};

/// `/proc/diskstats` 中一块整盘的累计扇区数；扇区固定为 512 字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskCounters {
    pub name: String,
    pub read_sectors: u64,
    pub write_sectors: u64,
}

/// 整盘判定：排除 loop/ram/sr/zram 与分区，只保留物理或虚拟整盘。
pub fn is_whole_disk(name: &str) -> bool {
    if ["loop", "ram", "sr", "zram", "dm-", "md"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return false;
    }
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return !name.contains('p');
    }
    !name.ends_with(|c: char| c.is_ascii_digit())
}

pub fn parse_diskstats(input: &str) -> Option<Vec<DiskCounters>> {
    let counters: Vec<DiskCounters> = input
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let name = *fields.get(2)?;
            if !is_whole_disk(name) {
                return None;
            }
            Some(DiskCounters {
                name: name.to_owned(),
                read_sectors: fields.get(5)?.parse().ok()?,
                write_sectors: fields.get(9)?.parse().ok()?,
            })
        })
        .collect();
    (!counters.is_empty()).then_some(counters)
}

pub fn disk_delta(
    before: &DiskCounters,
    after: &DiskCounters,
    elapsed: Duration,
) -> Option<DiskSample> {
    Some(DiskSample {
        read_bps: counter_rate(before.read_sectors, after.read_sectors, elapsed)? * 512,
        write_bps: counter_rate(before.write_sectors, after.write_sectors, elapsed)? * 512,
    })
}

/// 聚合所有整盘的速率；首帧、计数器重置或 `dt>5s` 时返回 `None`。
pub struct DiskSampler {
    root: PathBuf,
    previous: Option<BTreeMap<String, DiskCounters>>,
}

impl DiskSampler {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            previous: None,
        }
    }

    pub fn sample(&mut self, elapsed: Duration) -> io::Result<Option<DiskSample>> {
        let text = fs::read_to_string(self.root.join("diskstats"))?;
        let current: BTreeMap<String, DiskCounters> = parse_diskstats(&text)
            .unwrap_or_default()
            .into_iter()
            .map(|counters| (counters.name.clone(), counters))
            .collect();
        let previous = self.previous.replace(current.clone());
        let Some(previous) = previous else {
            return Ok(None);
        };
        let mut read_bps = 0u64;
        let mut write_bps = 0u64;
        let mut measured = false;
        for (name, after) in &current {
            let Some(before) = previous.get(name) else {
                continue;
            };
            let Some(delta) = disk_delta(before, after, elapsed) else {
                continue;
            };
            read_bps += delta.read_bps;
            write_bps += delta.write_bps;
            measured = true;
        }
        Ok(measured.then_some(DiskSample {
            read_bps,
            write_bps,
        }))
    }
}

impl Default for DiskSampler {
    fn default() -> Self {
        Self::new(Path::new("/proc"))
    }
}

/// 读取挂载点容量；不可读或不存在的挂载点被省略，而不是伪造零值。
pub fn read_filesystems(mounts: &[String]) -> Option<Vec<FilesystemSample>> {
    let samples: Vec<FilesystemSample> = mounts
        .iter()
        .filter_map(|mount| filesystem_usage(mount))
        .collect();
    (!samples.is_empty()).then_some(samples)
}

fn filesystem_usage(mount: &str) -> Option<FilesystemSample> {
    use std::ffi::CString;
    let path = CString::new(mount).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let total = (stat.f_blocks as u64).checked_mul(stat.f_frsize as u64)?;
    let available = (stat.f_bavail as u64).checked_mul(stat.f_frsize as u64)?;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    Some(FilesystemSample {
        mount: mount.to_owned(),
        total,
        available,
        used_pct: 100.0 * used as f32 / total as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const BEFORE: &str = " 8 0 sda 1 0 100 0 1 0 200 0 0 0 0\n 8 1 sda1 1 0 50 0 1 0 50 0 0 0 0\n 7 0 loop0 1 0 999 0 0 0 0 0 0 0 0\n";
    const AFTER: &str = " 8 0 sda 1 0 202 0 1 0 404 0 0 0 0\n";

    #[test]
    fn diskstats_uses_512_byte_sectors_and_whole_disks() {
        let before = parse_diskstats(BEFORE).unwrap();
        let after = parse_diskstats(AFTER).unwrap();
        assert_eq!(before.len(), 1);
        let sample = disk_delta(&before[0], &after[0], Duration::from_secs(1)).unwrap();
        assert_eq!(sample.read_bps, 102 * 512);
        assert_eq!(sample.write_bps, 204 * 512);
    }

    #[test]
    fn counter_reset_and_long_gap_return_none() {
        let before = parse_diskstats(BEFORE).unwrap();
        let reset = parse_diskstats(" 8 0 sda 1 0 10 0 1 0 20 0 0 0 0\n").unwrap();
        assert_eq!(
            disk_delta(&before[0], &reset[0], Duration::from_secs(1)),
            None
        );
        let after = parse_diskstats(AFTER).unwrap();
        assert_eq!(
            disk_delta(&before[0], &after[0], Duration::from_secs(6)),
            None
        );
        assert_eq!(disk_delta(&before[0], &after[0], Duration::ZERO), None);
    }

    #[test]
    fn partitions_and_virtual_devices_are_skipped() {
        assert!(is_whole_disk("sda"));
        assert!(is_whole_disk("nvme0n1"));
        assert!(is_whole_disk("mmcblk0"));
        assert!(!is_whole_disk("sda1"));
        assert!(!is_whole_disk("nvme0n1p1"));
        assert!(!is_whole_disk("mmcblk0p1"));
        assert!(!is_whole_disk("loop0"));
        assert!(!is_whole_disk("ram0"));
        assert!(!is_whole_disk("sr0"));
    }

    #[test]
    fn missing_mount_points_are_omitted() {
        let mounts = read_filesystems(&["/".to_string(), "/definitely/missing".to_string()])
            .expect("root filesystem is readable");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mount, "/");
        assert!(mounts[0].total > 0);
        assert!((0.0..=100.0).contains(&mounts[0].used_pct));
    }

    #[test]
    fn sampler_returns_none_on_the_baseline_frame() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("diskstats"), BEFORE).unwrap();
        let mut sampler = DiskSampler::new(dir.path());
        assert_eq!(sampler.sample(Duration::from_secs(1)).unwrap(), None);
        std::fs::write(dir.path().join("diskstats"), AFTER).unwrap();
        let sample = sampler.sample(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(sample.read_bps, 102 * 512);
        assert_eq!(sample.write_bps, 204 * 512);
    }
}
