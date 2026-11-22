//! 告警状态机：持续样本、冷却、静默时段、恢复与配置 reload。

use chrono::{DateTime, Utc};
use trafficview_core::config::AlertRule;
use trafficview_daemon::alerts::{AlertEngine, Metric};

fn rule(metric: &str, threshold: f64, duration_s: u32, cooldown_s: u32) -> AlertRule {
    AlertRule {
        enabled: true,
        metric: metric.into(),
        op: ">".into(),
        threshold,
        duration_s,
        cooldown_s,
        quiet_hours: String::new(),
        level: "warning".into(),
    }
}

fn ts(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
        .timestamp()
}

#[test]
fn threshold_requires_consecutive_samples() {
    let mut engine = AlertEngine::new(vec![rule("cpu_pct", 80.0, 3, 300)]);
    assert!(engine.observe_cpu(0, 90.0).is_empty());
    assert!(engine.observe_cpu(1, 95.0).is_empty());
    let events = engine.observe_cpu(2, 91.0);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].code, "cpu.high");
    assert!(events[0].active);
    assert!(engine.observe_cpu(3, 91.0).is_empty());

    // 掉回阈值以下产生一次恢复。
    let recovery = engine.observe_cpu(4, 50.0);
    assert_eq!(recovery.len(), 1);
    assert!(!recovery[0].active);
    assert_eq!(recovery[0].code, "cpu.high");
}

#[test]
fn recovery_rearms_rule_and_cooldown_limits_retrigger() {
    let mut engine = AlertEngine::new(vec![rule("cpu_pct", 80.0, 1, 300)]);
    assert_eq!(engine.observe_cpu(0, 90.0).len(), 1);
    assert!(!engine.observe_cpu(1, 50.0)[0].active);
    // 冷却未结束：状态进入 active，但不重复通知。
    assert!(engine.observe_cpu(100, 90.0).is_empty());
    assert!(!engine.observe_cpu(101, 50.0)[0].active);
    assert!(engine.observe_cpu(299, 90.0).is_empty(), "299 - 0 < 300");
    assert!(!engine.observe_cpu(299, 50.0)[0].active);
    // 冷却结束后再次触发才通知。
    assert_eq!(engine.observe_cpu(300, 90.0).len(), 1);
}

#[test]
fn quiet_hours_delay_notification_until_the_window_ends() {
    let mut quiet = rule("cpu_pct", 80.0, 1, 0);
    quiet.quiet_hours = "23:00-07:00".into();
    // UTC 时区，避免测试依赖本机时区。
    let mut engine = AlertEngine::with_offset_minutes(vec![quiet], 0);
    let late = ts("2026-11-20T23:30:00Z");
    assert!(
        engine.observe_cpu(late, 90.0).is_empty(),
        "quiet hours suppress"
    );
    // 静默期内继续越限也只更新状态。
    assert!(engine.observe_cpu(late + 60, 92.0).is_empty());
    let morning = ts("2026-11-21T07:01:00Z");
    let events = engine.observe_cpu(morning, 93.0);
    assert_eq!(events.len(), 1);
    assert!(events[0].active);
    assert!(events[0].message.contains("93.0"));
    assert!(
        engine.observe_cpu(morning + 60, 93.0).is_empty(),
        "already active"
    );
}

#[test]
fn disabled_rules_and_reloaded_rules_do_not_fire() {
    let mut disabled = rule("cpu_pct", 0.0, 1, 0);
    disabled.enabled = false;
    let mut engine = AlertEngine::new(vec![disabled]);
    assert!(engine.observe_cpu(0, 100.0).is_empty());

    let mut engine = AlertEngine::new(vec![rule("cpu_pct", 1.0, 1, 0)]);
    engine.reload(&[]);
    assert_eq!(engine.rules_len(), 0);
    assert!(engine.observe_cpu(10, 100.0).is_empty());
}

#[test]
fn all_supported_metrics_have_stable_codes() {
    let cases = [
        ("cpu_pct", Metric::CpuPct, "cpu.high"),
        ("mem_pct", Metric::MemPct, "mem.high"),
        ("net_rx_bps", Metric::NetRxBps, "net.rx.high"),
        ("net_tx_bps", Metric::NetTxBps, "net.tx.high"),
        ("month_bytes", Metric::MonthBytes, "month.quota"),
    ];
    for (name, metric, code) in cases {
        assert_eq!(Metric::parse(name), Some(metric));
        let mut engine = AlertEngine::new(vec![rule(name, 10.0, 1, 0)]);
        let events = engine.observe(metric, 0, 11.0);
        assert_eq!(events.len(), 1, "{name}");
        assert_eq!(events[0].code, code);
        assert_eq!(events[0].level, "warning");
    }
    assert_eq!(Metric::parse("unknown"), None);
}

#[test]
fn comparison_operators_are_honoured() {
    let mut below = rule("cpu_pct", 20.0, 1, 0);
    below.op = "<".into();
    let mut below = AlertEngine::new(vec![below]);
    assert_eq!(below.observe_cpu(0, 10.0).len(), 1);
    let recovered = below.observe_cpu(1, 20.0);
    assert_eq!(
        recovered.len(),
        1,
        "strictly less than must recover at the threshold"
    );
    assert!(!recovered[0].active);
    assert!(below.observe_cpu(2, 20.0).is_empty());

    let mut inclusive = rule("cpu_pct", 20.0, 1, 0);
    inclusive.op = "<=".into();
    let mut inclusive = AlertEngine::new(vec![inclusive]);
    assert_eq!(inclusive.observe_cpu(0, 20.0).len(), 1);
}
