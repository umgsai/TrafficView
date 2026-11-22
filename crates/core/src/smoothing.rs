//! 可选速率平滑：仅作用于展示快照，历史累计始终使用原始计数器。
//!
//! 首帧直接透传，不用 0 补齐窗口，避免启动瞬间显示异常低值。

use crate::config::NetworkSmoothing;

/// 双轴（上行/下行）速率平滑器。
#[derive(Debug, Clone)]
pub struct Smoother {
    kind: NetworkSmoothing,
    rx: Axis,
    tx: Axis,
}

#[derive(Debug, Clone, Default)]
struct Axis {
    window: [f64; 3],
    len: usize,
    pos: usize,
    ema: Option<f64>,
}

impl Axis {
    fn push(&mut self, value: f64) -> f64 {
        let value = if value.is_finite() && value >= 0.0 {
            value
        } else {
            0.0
        };
        self.window[self.pos] = value;
        self.pos = (self.pos + 1) % 3;
        self.len = (self.len + 1).min(3);
        value
    }

    fn moving_average(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        let sum: f64 = self.window[..self.len].iter().sum();
        sum / self.len as f64
    }

    fn ema(&mut self, value: f64) -> f64 {
        let next = match self.ema {
            Some(previous) => EMA_ALPHA * value + (1.0 - EMA_ALPHA) * previous,
            None => value,
        };
        self.ema = Some(next);
        next
    }

    fn reset(&mut self) {
        self.window = [0.0; 3];
        self.len = 0;
        self.pos = 0;
        self.ema = None;
    }
}

/// EMA 固定 alpha；不暴露配置以免历史/展示语义漂移。
pub const EMA_ALPHA: f64 = 0.3;

impl Smoother {
    pub fn new(kind: NetworkSmoothing) -> Self {
        Self {
            kind,
            rx: Axis::default(),
            tx: Axis::default(),
        }
    }

    pub fn kind(&self) -> NetworkSmoothing {
        self.kind
    }

    /// 传入一帧有效速率，返回展示用速率（四舍五入到字节/秒）。
    pub fn apply(&mut self, rx_bps: u64, tx_bps: u64) -> (u64, u64) {
        let rx = self.rx.push(rx_bps as f64);
        let tx = self.tx.push(tx_bps as f64);
        let (rx, tx) = match self.kind {
            NetworkSmoothing::None => (rx, tx),
            NetworkSmoothing::MovingAverage3 => (self.rx.moving_average(), self.tx.moving_average()),
            NetworkSmoothing::Ema => (self.rx.ema(rx), self.tx.ema(tx)),
        };
        (round(rx), round(tx))
    }

    /// 切换网卡或重建基线时清空历史窗口。
    pub fn reset(&mut self) {
        self.rx.reset();
        self.tx.reset();
    }
}

fn round(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else {
        value.round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_average_of_three_points_is_the_middle_value() {
        let mut smoother = Smoother::new(NetworkSmoothing::MovingAverage3);
        assert_eq!(smoother.apply(100, 100), (100, 100));
        assert_eq!(smoother.apply(200, 200), (150, 150));
        assert_eq!(smoother.apply(300, 300), (200, 200));
    }

    #[test]
    fn ema_with_alpha_0_3_moves_towards_the_new_value() {
        let mut smoother = Smoother::new(NetworkSmoothing::Ema);
        assert_eq!(smoother.apply(100, 100), (100, 100));
        assert_eq!(smoother.apply(0, 0), (70, 70));
    }

    #[test]
    fn first_frame_is_passed_through_without_zero_padding() {
        let mut smoother = Smoother::new(NetworkSmoothing::MovingAverage3);
        assert_eq!(smoother.apply(4096, 8192), (4096, 8192));
    }

    #[test]
    fn none_passes_values_through_unchanged() {
        let mut smoother = Smoother::new(NetworkSmoothing::None);
        assert_eq!(smoother.apply(5, 7), (5, 7));
        assert_eq!(smoother.apply(0, 0), (0, 0));
    }

    #[test]
    fn reset_clears_the_window_and_ema_state() {
        let mut smoother = Smoother::new(NetworkSmoothing::Ema);
        smoother.apply(100, 100);
        smoother.reset();
        assert_eq!(smoother.apply(10, 20), (10, 20));
    }
}
