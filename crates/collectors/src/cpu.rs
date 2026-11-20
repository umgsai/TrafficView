use std::{
    fs, io,
    path::{Path, PathBuf},
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

    pub fn sample(&mut self) -> io::Result<Option<(f32, Vec<f32>)>> {
        let current = parse_proc_stat(&fs::read_to_string(self.root.join("stat"))?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc/stat"))?;
        let previous = self.previous.replace(current.clone());
        Ok(previous.and_then(|old| cpu_usage(&old, &current)))
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

    #[test]
    fn computes_total_and_per_core_usage() {
        let before = parse_proc_stat("cpu 100 0 100 800 0 0 0 0 0 0\ncpu0 100 0 100 800 0 0 0 0 0 0\n").unwrap();
        let after = parse_proc_stat("cpu 150 0 150 1100 0 0 0 0 0 0\ncpu0 150 0 150 1100 0 0 0 0 0 0\n").unwrap();
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
}
