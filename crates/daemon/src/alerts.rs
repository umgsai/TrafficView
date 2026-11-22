//! 告警规则状态机：持续时长、冷却、静默时段与恢复。
//!
//! 规则与通知发送分离：状态机只产生 `AlertEvent`，桌面通知由阻塞线程调用
//! `org.freedesktop.Notifications`，无服务时仅记录 warning，不影响 daemon。

use std::collections::HashMap;

use chrono::Local;
use trafficview_core::{
    config::{parse_hhmm, AlertRule},
    AlertEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Metric {
    CpuPct,
    MemPct,
    NetRxBps,
    NetTxBps,
    MonthBytes,
}

impl Metric {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cpu_pct" => Some(Self::CpuPct),
            "mem_pct" => Some(Self::MemPct),
            "net_rx_bps" => Some(Self::NetRxBps),
            "net_tx_bps" => Some(Self::NetTxBps),
            "month_bytes" => Some(Self::MonthBytes),
            _ => None,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::CpuPct => "cpu.high",
            Self::MemPct => "mem.high",
            Self::NetRxBps => "net.rx.high",
            Self::NetTxBps => "net.tx.high",
            Self::MonthBytes => "month.quota",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CpuPct => "CPU 使用率",
            Self::MemPct => "内存使用率",
            Self::NetRxBps => "下行速率",
            Self::NetTxBps => "上行速率",
            Self::MonthBytes => "本月流量",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    fn parse(value: &str) -> Option<Self> {
        match value {
            ">" => Some(Self::Gt),
            ">=" => Some(Self::Ge),
            "<" => Some(Self::Lt),
            "<=" => Some(Self::Le),
            _ => None,
        }
    }

    fn hit(self, value: f64, threshold: f64) -> bool {
        match self {
            Self::Gt => value > threshold,
            Self::Ge => value >= threshold,
            Self::Lt => value < threshold,
            Self::Le => value <= threshold,
        }
    }
}

#[derive(Debug, Clone)]
struct RuleState {
    rule: AlertRule,
    metric: Metric,
    op: Op,
    /// 连续命中的样本数；计数语义与默认 1s 采样一致。
    consecutive: u32,
    active: bool,
    /// 进入 active 但被静默或冷却拦住的通知。
    pending: bool,
    last_notified: Option<i64>,
}

/// 一个进程内的告警状态机。
pub struct AlertEngine {
    rules: Vec<RuleState>,
    /// 静默时段按此偏移解释墙上时间；默认系统本地时区。
    offset_minutes: i32,
}

impl AlertEngine {
    pub fn new(rules: Vec<AlertRule>) -> Self {
        Self {
            rules: rules.into_iter().filter_map(build).collect(),
            offset_minutes: local_offset_minutes(),
        }
    }

    /// 测试/显式时区；静默时段使用该偏移。
    pub fn with_offset_minutes(rules: Vec<AlertRule>, offset_minutes: i32) -> Self {
        Self {
            rules: rules.into_iter().filter_map(build).collect(),
            offset_minutes,
        }
    }

    /// 配置热加载：不再存在的规则被清理，状态从头建立。
    pub fn reload(&mut self, rules: &[AlertRule]) {
        self.rules = rules.iter().cloned().filter_map(build).collect();
    }

    pub fn rules_len(&self) -> usize {
        self.rules.len()
    }

    pub fn observe_cpu(&mut self, ts_secs: i64, value: f64) -> Vec<AlertEvent> {
        self.observe(Metric::CpuPct, ts_secs, value)
    }

    pub fn observe_mem(&mut self, ts_secs: i64, value: f64) -> Vec<AlertEvent> {
        self.observe(Metric::MemPct, ts_secs, value)
    }

    /// 喂入某一指标的一帧；返回需要广播/通知的事件（含恢复）。
    pub fn observe(&mut self, metric: Metric, ts_secs: i64, value: f64) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        let minute = minute_of_day(ts_secs, self.offset_minutes);
        for state in &mut self.rules {
            if state.metric != metric || !state.rule.enabled {
                continue;
            }
            let quiet = !state.rule.quiet_hours.is_empty()
                && in_quiet_hours(&state.rule.quiet_hours, minute);
            let hit = state.op.hit(value, state.rule.threshold);
            if hit {
                state.consecutive = state.consecutive.saturating_add(1);
                if state.active {
                    if state.pending && !quiet {
                        state.pending = false;
                        state.last_notified = Some(ts_secs);
                        events.push(event(state, ts_secs, value, true));
                    }
                    continue;
                }
                let required = state.rule.duration_s.max(1);
                if state.consecutive < required {
                    continue;
                }
                state.active = true;
                if quiet {
                    state.pending = true;
                } else if cooldown_elapsed(state, ts_secs) {
                    state.last_notified = Some(ts_secs);
                    events.push(event(state, ts_secs, value, true));
                } else {
                    // 冷却期内重新越限：保持 active，但不再打扰。
                    state.pending = false;
                }
            } else {
                state.consecutive = 0;
                if state.active {
                    state.active = false;
                    state.pending = false;
                    events.push(event(state, ts_secs, value, false));
                }
            }
        }
        events
    }
}

fn build(rule: AlertRule) -> Option<RuleState> {
    let metric = Metric::parse(&rule.metric)?;
    let op = Op::parse(&rule.op)?;
    Some(RuleState {
        rule,
        metric,
        op,
        consecutive: 0,
        active: false,
        pending: false,
        last_notified: None,
    })
}

fn cooldown_elapsed(state: &RuleState, ts_secs: i64) -> bool {
    state
        .last_notified
        .is_none_or(|last| ts_secs - last >= i64::from(state.rule.cooldown_s))
}

fn event(state: &RuleState, ts_secs: i64, value: f64, active: bool) -> AlertEvent {
    let code = state.metric.code();
    let message = if active {
        format!(
            "{} {:.1} 超过阈值 {:.1}（持续 {} 秒）",
            state.metric.label(),
            value,
            state.rule.threshold,
            state.rule.duration_s
        )
    } else {
        format!("{} 已恢复（当前 {:.1}）", state.metric.label(), value)
    };
    AlertEvent {
        level: if active {
            state.rule.level.clone()
        } else {
            "info".into()
        },
        code: code.into(),
        message,
        active,
        ts_secs,
    }
}

fn local_offset_minutes() -> i32 {
    Local::now().offset().local_minus_utc() / 60
}

fn minute_of_day(ts_secs: i64, offset_minutes: i32) -> u32 {
    let local = ts_secs + i64::from(offset_minutes) * 60;
    local.rem_euclid(86_400) as u32 / 60
}

/// `HH:MM-HH:MM`，允许跨午夜。
pub fn in_quiet_hours(spec: &str, minute_of_day: u32) -> bool {
    let Some((start, end)) = spec.split_once('-') else {
        return false;
    };
    let (Some(start), Some(end)) = (parse_hhmm(start), parse_hhmm(end)) else {
        return false;
    };
    if start <= end {
        (start..end).contains(&minute_of_day)
    } else {
        minute_of_day >= start || minute_of_day < end
    }
}

/// 通过会话总线发送桌面通知；无通知服务时返回错误由调用方记录 warning。
pub fn send_desktop_notification(event: &AlertEvent) -> anyhow::Result<()> {
    if !event.active {
        return Ok(());
    }
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    let actions: Vec<&str> = Vec::new();
    let hints: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
    let _: u32 = proxy
        .call_method(
            "Notify",
            &(
                "TrafficView",
                0u32,
                "dialog-information",
                "TrafficView 告警",
                event.message.as_str(),
                actions,
                hints,
                -1i32,
            ),
        )?
        .body()
        .deserialize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hours_cross_midnight() {
        assert!(in_quiet_hours("23:00-07:00", 23 * 60));
        assert!(in_quiet_hours("23:00-07:00", 6 * 60));
        assert!(!in_quiet_hours("23:00-07:00", 12 * 60));
        assert!(in_quiet_hours("09:00-17:00", 12 * 60));
        assert!(!in_quiet_hours("09:00-17:00", 20 * 60));
        assert!(!in_quiet_hours("bad", 0));
    }
}
