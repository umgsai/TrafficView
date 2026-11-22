use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuTimes {
    total: u64,
    busy: u64,
}

pub fn parse_proc_stat(input: &str) -> Option<Vec<CpuTimes>> {
    let rows: Vec<CpuTimes> = input
        .lines()
        .filter(|line| line.starts_with("cpu"))
        .filter_map(|line| {
            let values: Option<Vec<u64>> = line
                .split_whitespace()
                .skip(1)
                .take(8)
                .map(|v| v.parse::<u64>().ok())
                .collect();
            let values = values?;
            if values.len() < 8 {
                return None;
            }
            let total = values.iter().sum();
            let busy = total - values[3] - values[4];
            Some(CpuTimes { total, busy })
        })
        .collect();
    (!rows.is_empty()).then_some(rows)
}

pub fn cpu_usage(before: &[CpuTimes], after: &[CpuTimes]) -> Option<(f32, Vec<f32>)> {
    if before.len() != after.len() {
        return None;
    }
    let usage: Option<Vec<f32>> = before
        .iter()
        .zip(after)
        .map(|(a, b)| {
            let total = b.total.checked_sub(a.total)?;
            let busy = b.busy.checked_sub(a.busy)?;
            (total > 0).then_some(100.0 * busy as f32 / total as f32)
        })
        .collect();
    let usage = usage?;
    Some((usage[0], usage[1..].to_vec()))
}

/// 读取 `cpu*/cpufreq/scaling_cur_freq`（kHz）并返回可读核心的平均 MHz。
pub fn read_freq_mhz(sys_cpu_root: &Path) -> Option<u32> {
    let mut total = 0u64;
    let mut count = 0u64;
    for entry in fs::read_dir(sys_cpu_root).ok()?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("cpu") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path().join("cpufreq/scaling_cur_freq")) else {
            continue;
        };
        let Ok(khz) = text.trim().parse::<u64>() else {
            continue;
        };
        if khz == 0 {
            continue;
        }
        total += khz;
        count += 1;
    }
    (count > 0).then(|| (total / count / 1000) as u32)
}

/// 解析 `/proc/loadavg` 的前三个浮点数。
pub fn parse_loadavg(input: &str) -> Option<[f32; 3]> {
    let mut fields = input.split_whitespace();
    let mut load = [0.0f32; 3];
    for slot in &mut load {
        let value: f32 = fields.next()?.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        *slot = value;
    }
    Some(load)
}

pub fn read_loadavg(proc_root: &Path) -> Option<[f32; 3]> {
    parse_loadavg(&fs::read_to_string(proc_root.join("loadavg")).ok()?)
}

pub struct ProcCpu {
    root: PathBuf,
    previous: Option<Vec<CpuTimes>>,
}

impl ProcCpu {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            previous: None,
        }
    }

    pub fn sample(&mut self, elapsed: Duration) -> io::Result<Option<(f32, Vec<f32>)>> {
        let current = parse_proc_stat(&fs::read_to_string(self.root.join("stat"))?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/stat"))?;
        let previous = self.previous.replace(current.clone());
        let Some(old) = previous else {
            return Ok(None);
        };
        if elapsed.is_zero() || elapsed > Duration::from_secs(5) {
            return Ok(None);
        }
        Ok(cpu_usage(&old, &current))
    }
}

impl Default for ProcCpu {
    fn default() -> Self {
        Self::new(Path::new("/proc"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    #[test]
    fn computes_total_and_per_core_usage() {
        let before =
            parse_proc_stat("cpu 100 0 100 800 0 0 0 0 0 0\ncpu0 100 0 100 800 0 0 0 0 0 0\n")
                .unwrap();
        let after =
            parse_proc_stat("cpu 150 0 150 1100 0 0 0 0 0 0\ncpu0 150 0 150 1100 0 0 0 0 0 0\n")
                .unwrap();
        let (total, cores) = cpu_usage(&before, &after).unwrap();
        assert_eq!(total, 25.0);
        assert_eq!(cores, vec![25.0]);
    }

    #[test]
    fn rejects_boot_counter_reset() {
        let before = parse_proc_stat("cpu 100 0 100 800 0 0 0 0 0 0\n").unwrap();
        let after = parse_proc_stat("cpu 1 0 1 8 0 0 0 0 0 0\n").unwrap();
        assert_eq!(cpu_usage(&before, &after), None);
    }

    fn write_stat(dir: &Path, line: &str) {
        fs::write(dir.join("stat"), format!("{line}\ncpu0 {}\n", &line[4..])).unwrap();
    }

    #[test]
    fn long_gap_discards_frame_and_rebuilds_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let mut cpu = ProcCpu::new(dir.path());
        write_stat(dir.path(), "cpu 100 0 100 800 0 0 0 0 0 0");
        assert_eq!(cpu.sample(Duration::ZERO).unwrap(), None);
        write_stat(dir.path(), "cpu 150 0 150 1100 0 0 0 0 0 0");
        assert_eq!(cpu.sample(Duration::from_secs(6)).unwrap(), None);
        write_stat(dir.path(), "cpu 200 0 200 1200 0 0 0 0 0 0");
        let (total, cores) = cpu.sample(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(total, 50.0);
        assert_eq!(cores, vec![50.0]);
    }

    #[test]
    fn cpufreq_averages_only_readable_cores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (cpu, freq) in [
            ("cpu0", Some("2000000")),
            ("cpu1", Some("1800000")),
            ("cpu2", None),
        ] {
            let cpufreq = root.join(cpu).join("cpufreq");
            if let Some(freq) = freq {
                fs::create_dir_all(&cpufreq).unwrap();
                fs::write(cpufreq.join("scaling_cur_freq"), format!("{freq}\n")).unwrap();
            }
        }
        assert_eq!(read_freq_mhz(root), Some(1900));
        assert_eq!(read_freq_mhz(&root.join("missing")), None);
    }

    #[test]
    fn loadavg_reports_three_averages() {
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 1/1234 5678\n"),
            Some([0.52, 0.58, 0.59])
        );
        assert_eq!(parse_loadavg("0.52 0.58\n"), None);
        assert_eq!(parse_loadavg("nan 0.1 0.2\n"), None);
    }

    #[test]
    fn zero_elapsed_discards_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mut cpu = ProcCpu::new(dir.path());
        write_stat(dir.path(), "cpu 100 0 100 800 0 0 0 0 0 0");
        assert_eq!(cpu.sample(Duration::ZERO).unwrap(), None);
        write_stat(dir.path(), "cpu 150 0 150 1100 0 0 0 0 0 0");
        assert_eq!(cpu.sample(Duration::ZERO).unwrap(), None);
        write_stat(dir.path(), "cpu 200 0 200 1200 0 0 0 0 0 0");
        assert!(cpu.sample(Duration::from_secs(1)).unwrap().is_some());
    }
}
