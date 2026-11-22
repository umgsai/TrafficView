use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use trafficview_core::GpuSample;

const INTEL_SCAN_INTERVAL: Duration = Duration::from_secs(5);
const INTEL_STALE_AFTER: Duration = Duration::from_secs(15);
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(2);

/// AMD：直接读取 `card*/device` 下的 `gpu_busy_percent` 与 VRAM 计数。
pub fn read_amd_gpus(drm_root: &Path) -> Vec<GpuSample> {
    let Ok(entries) = fs::read_dir(drm_root) else {
        return Vec::new();
    };
    let mut cards: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("card") && !name.contains('-')
        })
        .collect();
    cards.sort();
    cards
        .iter()
        .enumerate()
        .filter_map(|(index, dir)| amd_card(dir, index))
        .collect()
}

fn amd_card(dir: &Path, index: usize) -> Option<GpuSample> {
    let device = dir.join("device");
    if read_trimmed(&device.join("vendor"))? != "0x1002" {
        return None;
    }
    let util_pct: f32 = read_trimmed(&device.join("gpu_busy_percent"))?
        .parse()
        .ok()?;
    if !(0.0..=100.0).contains(&util_pct) {
        return None;
    }
    Some(GpuSample {
        name: read_trimmed(&device.join("product_name"))
            .unwrap_or_else(|| format!("AMD GPU {index}")),
        util_pct,
        mem_used: read_u64(&device.join("mem_info_vram_used")).unwrap_or(0),
        mem_total: read_u64(&device.join("mem_info_vram_total")).unwrap_or(0),
        freq_mhz: None,
        stale: false,
        observed_at_ms: None,
    })
}

/// NVIDIA `nvidia-smi` CSV：`index,name,utilization.gpu,memory.used,temperature.gpu`。
pub fn parse_nvidia_smi(input: &str) -> Vec<GpuSample> {
    input
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 5 || fields[1].is_empty() {
                return None;
            }
            let util_pct: f32 = fields[2].parse().ok()?;
            let used_mib: u64 = fields[3].parse().ok()?;
            if !(0.0..=100.0).contains(&util_pct) {
                return None;
            }
            Some(GpuSample {
                name: fields[1].to_owned(),
                util_pct,
                mem_used: used_mib.saturating_mul(1024 * 1024),
                // 该查询不包含 memory.total；未采集时保留 0 并依赖 CollectorStatus 解释。
                mem_total: 0,
                freq_mhz: None,
                stale: false,
                observed_at_ms: None,
            })
        })
        .collect()
}

fn read_nvidia_gpus() -> Result<Vec<GpuSample>, String> {
    match unsafe { read_nvml_gpus() } {
        Ok(gpus) if !gpus.is_empty() => Ok(gpus),
        Ok(_) => read_nvidia_smi(),
        Err(nvml_error) => read_nvidia_smi()
            .map_err(|smi_error| format!("nvml: {nvml_error}; nvidia-smi: {smi_error}")),
    }
}

fn read_nvidia_smi() -> Result<Vec<GpuSample>, String> {
    let mut child = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,utilization.gpu,memory.used,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("nvidia-smi unavailable: {error}"))?;
    let deadline = Instant::now() + NVIDIA_SMI_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("nvidia-smi timed out".into());
            }
            Err(error) => return Err(format!("nvidia-smi failed: {error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("nvidia-smi output failed: {error}"))?;
    let gpus = parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout));
    if gpus.is_empty() {
        return Err("nvidia-smi returned no usable rows".into());
    }
    Ok(gpus)
}

#[repr(C)]
struct NvmlUtilization {
    gpu: u32,
    _memory: u32,
}

#[repr(C)]
struct NvmlMemory {
    total: u64,
    _free: u64,
    used: u64,
}

unsafe fn nvml_symbol<T: Copy>(
    handle: *mut libc::c_void,
    name: &std::ffi::CStr,
) -> Result<T, String> {
    let symbol = libc::dlsym(handle, name.as_ptr());
    if symbol.is_null() {
        return Err(format!("missing NVML symbol {}", name.to_string_lossy()));
    }
    Ok(std::mem::transmute_copy(&symbol))
}

/// 优先 NVML 动态库；未安装或初始化失败时由调用方回退到 `nvidia-smi`。
unsafe fn read_nvml_gpus() -> Result<Vec<GpuSample>, String> {
    let library = libc::dlopen(c"libnvidia-ml.so.1".as_ptr(), libc::RTLD_LAZY);
    if library.is_null() {
        return Err("libnvidia-ml.so.1 is not loadable".into());
    }
    let result = (|| -> Result<Vec<GpuSample>, String> {
        let init: unsafe extern "C" fn() -> i32 = nvml_symbol(library, c"nvmlInit_v2")?;
        let shutdown: unsafe extern "C" fn() -> i32 = nvml_symbol(library, c"nvmlShutdown")?;
        let count: unsafe extern "C" fn(*mut u32) -> i32 =
            nvml_symbol(library, c"nvmlDeviceGetCount_v2")?;
        let handle_by_index: unsafe extern "C" fn(u32, *mut *mut libc::c_void) -> i32 =
            nvml_symbol(library, c"nvmlDeviceGetHandleByIndex_v2")?;
        let device_name: unsafe extern "C" fn(*mut libc::c_void, *mut libc::c_char, u32) -> i32 =
            nvml_symbol(library, c"nvmlDeviceGetName")?;
        let utilization: unsafe extern "C" fn(*mut libc::c_void, *mut NvmlUtilization) -> i32 =
            nvml_symbol(library, c"nvmlDeviceGetUtilizationRates")?;
        let memory: unsafe extern "C" fn(*mut libc::c_void, *mut NvmlMemory) -> i32 =
            nvml_symbol(library, c"nvmlDeviceGetMemoryInfo")?;

        if init() != 0 {
            return Err("nvmlInit failed (driver mismatch?)".into());
        }
        let mut devices = 0u32;
        let mut gpus = Vec::new();
        if count(&mut devices) == 0 {
            for index in 0..devices {
                let mut device: *mut libc::c_void = std::ptr::null_mut();
                if handle_by_index(index, &mut device) != 0 {
                    continue;
                }
                let mut buffer = [0 as libc::c_char; 96];
                if device_name(device, buffer.as_mut_ptr(), 96) != 0 {
                    continue;
                }
                let name = std::ffi::CStr::from_ptr(buffer.as_ptr())
                    .to_string_lossy()
                    .into_owned();
                let mut utilization_rates = NvmlUtilization { gpu: 0, _memory: 0 };
                let util_pct = if utilization(device, &mut utilization_rates) == 0 {
                    utilization_rates.gpu as f32
                } else {
                    0.0
                };
                let mut memory_info = NvmlMemory {
                    total: 0,
                    _free: 0,
                    used: 0,
                };
                let (mem_used, mem_total) = if memory(device, &mut memory_info) == 0 {
                    (memory_info.used, memory_info.total)
                } else {
                    (0, 0)
                };
                gpus.push(GpuSample {
                    name,
                    util_pct,
                    mem_used,
                    mem_total,
                    freq_mhz: None,
                    stale: false,
                    observed_at_ms: None,
                });
            }
        }
        shutdown();
        Ok(gpus)
    })();
    libc::dlclose(library);
    result
}

/// 解析单个 fdinfo 的首个 `drm-engine-*` 纳秒计数。
pub fn parse_intel_fdinfo(input: &str) -> Option<u64> {
    input.lines().find_map(|line| {
        let (key, rest) = line.split_once(':')?;
        if !key.trim().starts_with("drm-engine-") {
            return None;
        }
        let value = rest.trim().strip_suffix("ns")?.trim();
        value.parse().ok()
    })
}

pub fn intel_engine_usage(before: u64, after: u64, elapsed: Duration) -> Option<f32> {
    if elapsed.is_zero() || after < before {
        return None;
    }
    let usage = (after - before) as f64 / elapsed.as_nanos() as f64 * 100.0;
    Some(usage.min(100.0) as f32)
}

fn scan_intel_engine_ns(proc_root: &Path) -> u64 {
    let Ok(pids) = fs::read_dir(proc_root) else {
        return 0;
    };
    let mut total = 0u64;
    for pid in pids.flatten() {
        if !pid
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let Ok(fds) = fs::read_dir(pid.path().join("fdinfo")) else {
            continue;
        };
        // 同一进程可能有多个 fd 指向同一 DRM 文件；取最大值即可去重。
        let mut best = 0u64;
        for fd in fds.flatten() {
            let Ok(text) = fs::read_to_string(fd.path()) else {
                continue;
            };
            if let Some(ns) = parse_intel_fdinfo(&text) {
                best = best.max(ns);
            }
        }
        total += best;
    }
    total
}

/// Intel 每 5 秒扫描一次 fdinfo；中间帧返回 `stale=true` 的上一读数。
pub struct IntelScanner {
    proc_root: PathBuf,
    last_scan: Option<Instant>,
    last_total: Option<u64>,
    last_sample: Option<GpuSample>,
    last_observed: Option<Instant>,
}

impl IntelScanner {
    pub fn new(proc_root: impl Into<PathBuf>) -> Self {
        Self {
            proc_root: proc_root.into(),
            last_scan: None,
            last_total: None,
            last_sample: None,
            last_observed: None,
        }
    }

    pub fn sample(&mut self, now: Instant, observed_at_ms: u64) -> Option<GpuSample> {
        let due = self
            .last_scan
            .is_none_or(|last| now.duration_since(last) >= INTEL_SCAN_INTERVAL);
        if due {
            let elapsed = self.last_scan.map(|last| now.duration_since(last));
            let total = scan_intel_engine_ns(&self.proc_root);
            self.last_scan = Some(now);
            if let (Some(previous), Some(elapsed)) = (self.last_total, elapsed) {
                if let Some(util_pct) = intel_engine_usage(previous, total, elapsed) {
                    self.last_sample = Some(GpuSample {
                        name: "Intel GPU".into(),
                        util_pct,
                        mem_used: 0,
                        mem_total: 0,
                        freq_mhz: None,
                        stale: false,
                        observed_at_ms: Some(observed_at_ms),
                    });
                    self.last_observed = Some(now);
                }
            }
            self.last_total = Some(total);
        }
        let last_observed = self.last_observed?;
        if now.duration_since(last_observed) > INTEL_STALE_AFTER {
            return None;
        }
        let mut sample = self.last_sample.clone()?;
        sample.stale = !due;
        Some(sample)
    }
}

/// 后端优先顺序：NVIDIA（NVML，失败回退 nvidia-smi）→ AMD sysfs → Intel fdinfo。
pub struct GpuSampler {
    drm_root: PathBuf,
    intel: IntelScanner,
    nvidia_device: PathBuf,
}

impl GpuSampler {
    pub fn new(drm_root: impl Into<PathBuf>, proc_root: impl Into<PathBuf>) -> Self {
        Self::with_nvidia_device(drm_root, proc_root, Path::new("/dev/nvidiactl"))
    }

    pub fn with_nvidia_device(
        drm_root: impl Into<PathBuf>,
        proc_root: impl Into<PathBuf>,
        nvidia_device: impl Into<PathBuf>,
    ) -> Self {
        Self {
            drm_root: drm_root.into(),
            intel: IntelScanner::new(proc_root),
            nvidia_device: nvidia_device.into(),
        }
    }

    /// 返回 `(gpus, 不可用原因)`；有可用后端时原因为 `None`。
    pub fn sample(
        &mut self,
        now: Instant,
        observed_at_ms: u64,
    ) -> (Vec<GpuSample>, Option<String>) {
        if self.nvidia_device.exists() {
            return match read_nvidia_gpus() {
                Ok(gpus) if !gpus.is_empty() => (gpus, None),
                Ok(_) => (
                    Vec::new(),
                    Some("nvidia backend returned no devices".into()),
                ),
                Err(error) => (Vec::new(), Some(error)),
            };
        }
        let amd = read_amd_gpus(&self.drm_root);
        if !amd.is_empty() {
            return (amd, None);
        }
        if has_intel_card(&self.drm_root) {
            return match self.intel.sample(now, observed_at_ms) {
                Some(gpu) => (vec![gpu], None),
                None => (Vec::new(), Some("intel engine counters unavailable".into())),
            };
        }
        (Vec::new(), Some("no supported GPU backend".into()))
    }
}

fn has_intel_card(drm_root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(drm_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        name.starts_with("card")
            && !name.contains('-')
            && read_trimmed(&entry.path().join("device/vendor")).as_deref() == Some("0x8086")
    })
}

fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn write(path: &Path, value: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    fn amd_card(drm: &Path, index: usize, busy: Option<&str>) {
        let device = drm.join(format!("card{index}/device"));
        write(&device.join("vendor"), "0x1002\n");
        write(&device.join("mem_info_vram_used"), "536870912\n");
        write(&device.join("mem_info_vram_total"), "4294967296\n");
        if let Some(busy) = busy {
            write(&device.join("gpu_busy_percent"), busy);
        }
    }

    #[test]
    fn amd_backend_reads_busy_and_vram() {
        let dir = tempfile::tempdir().unwrap();
        amd_card(dir.path(), 0, Some("37\n"));
        let gpus = read_amd_gpus(dir.path());
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].util_pct, 37.0);
        assert_eq!(gpus[0].mem_used, 536_870_912);
        assert_eq!(gpus[0].mem_total, 4_294_967_296);
        assert!(!gpus[0].stale);
    }

    #[test]
    fn amd_card_without_busy_counter_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        amd_card(dir.path(), 0, None);
        assert!(read_amd_gpus(dir.path()).is_empty());
    }

    #[test]
    fn nvidia_smi_rows_keep_mebibytes_converted_to_bytes() {
        let text = "0, NVIDIA GeForce RTX 4070, 42, 1024, 55\n";
        let gpus = parse_nvidia_smi(text);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4070");
        assert_eq!(gpus[0].util_pct, 42.0);
        assert_eq!(gpus[0].mem_used, 1024 * 1024 * 1024);
    }

    #[test]
    fn nvidia_driver_errors_produce_no_rows() {
        let text = "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver\n";
        assert!(parse_nvidia_smi(text).is_empty());
    }

    #[test]
    fn intel_utilization_uses_monotonic_engine_deltas() {
        let before = parse_intel_fdinfo("drm-engine-render: 1000000000 ns\n").unwrap();
        let after = parse_intel_fdinfo("drm-engine-render: 1500000000 ns\n").unwrap();
        assert_eq!(
            intel_engine_usage(before, after, Duration::from_secs(1)),
            Some(50.0)
        );
        assert_eq!(
            intel_engine_usage(after, before, Duration::from_secs(1)),
            None
        );
    }

    fn write_fdinfo(proc_root: &Path, pid: &str, fd: &str, ns: u64) {
        write(
            &proc_root.join(pid).join("fdinfo").join(fd),
            &format!("drm-engine-render: {ns} ns\n"),
        );
    }

    #[test]
    fn intel_scanner_dedupes_fds_and_expires_stale_readings() {
        let dir = tempfile::tempdir().unwrap();
        let proc_root = dir.path();
        let mut scanner = IntelScanner::new(proc_root);
        let start = Instant::now();
        write_fdinfo(proc_root, "100", "3", 1_000_000_000);
        assert_eq!(scanner.sample(start, 1_000), None);

        write_fdinfo(proc_root, "100", "3", 2_000_000_000);
        write_fdinfo(proc_root, "100", "4", 1_000_000_000);
        let fresh = scanner
            .sample(start + Duration::from_secs(5), 6_000)
            .unwrap();
        assert_eq!(fresh.util_pct, 20.0);
        assert!(!fresh.stale);

        fs::remove_dir_all(proc_root.join("100")).unwrap();
        let stale = scanner
            .sample(start + Duration::from_secs(6), 7_000)
            .unwrap();
        assert!(stale.stale);
        assert_eq!(stale.observed_at_ms, Some(6_000));

        assert_eq!(
            scanner.sample(start + Duration::from_secs(21), 22_000),
            None
        );
    }

    #[test]
    fn unsupported_hardware_reports_a_reason() {
        let drm = tempfile::tempdir().unwrap();
        let proc_root = tempfile::tempdir().unwrap();
        let missing = drm.path().join("no-nvidia-here");
        let mut sampler = GpuSampler::with_nvidia_device(drm.path(), proc_root.path(), missing);
        let (gpus, reason) = sampler.sample(Instant::now(), 1_000);
        assert!(gpus.is_empty());
        assert_eq!(reason.as_deref(), Some("no supported GPU backend"));
    }
}
