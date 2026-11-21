//! 采样时间基准：`CLOCK_MONOTONIC`，不受系统墙上时钟调整影响。

/// 自 `CLOCK_MONOTONIC` 零点（通常是开机，不含休眠时间）起的毫秒数。
///
/// 单调递增，不随 `CLOCK_REALTIME` 跳变；不同进程得到的是同一基准下的绝对值。
#[cfg(unix)]
pub fn monotonic_ms() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 指针指向栈上已初始化的 timespec，CLOCK_MONOTONIC 在所有 Linux 上可用。
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_MONOTONIC) failed");
    now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000
}

/// 非 Unix 平台没有 `CLOCK_MONOTONIC`，仅保证可编译；本项目运行目标只有 Linux。
#[cfg(not(unix))]
pub fn monotonic_ms() -> u64 {
    use std::{sync::OnceLock, time::Instant};
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn advances_in_milliseconds() {
        let first = monotonic_ms();
        std::thread::sleep(Duration::from_millis(30));
        let second = monotonic_ms();
        let delta = second - first;
        assert!((25..=1000).contains(&delta), "delta={delta}ms");
    }

    #[test]
    fn is_not_wall_clock_epoch() {
        let monotonic = monotonic_ms();
        let wall = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(monotonic > 0);
        assert!(
            monotonic < wall,
            "monotonic_ms ({monotonic}) must not follow the wall clock ({wall})"
        );
    }
}
