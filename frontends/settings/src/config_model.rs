//! 设置窗口使用的配置模型：在 daemon `GetConfig` 返回的 `a{sv}` 与界面控件之间转换。
//!
//! 这里**重复**了 daemon `dbus.rs` 中 `config_to_dbus`/`apply_config_update` 的字段映射，
//! 目的是让设置界面成为独立 workspace（根 workspace 用 exclude 排除，不解析 GTK 依赖）。
//! D-Bus v1 契约已冻结，键名/类型与 daemon 侧一一对应；单测覆盖往返以避免漂移。

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};

/// 界面编辑的一条悬浮窗显示项。
#[derive(Debug, Clone, PartialEq)]
pub struct UiItem {
    pub id: String,
    pub label: String,
    pub unit: String,
    pub format: String,
    pub color: String,
    pub visible: bool,
}

/// 界面编辑的一条告警规则。
#[derive(Debug, Clone, PartialEq)]
pub struct UiAlertRule {
    pub enabled: bool,
    pub metric: String,
    pub op: String,
    pub threshold: f64,
    pub duration_s: u32,
    pub cooldown_s: u32,
    pub quiet_hours: String,
    pub level: String,
}

/// 界面持有的完整配置快照；缺失字段按 `Config::default()` 兜底。
#[derive(Debug, Clone, PartialEq)]
pub struct UiConfig {
    pub sample_interval_ms: u32,
    pub active_interface: String,
    pub memory_include_cache: bool,
    pub interface_blacklist: Vec<String>,
    pub interface_whitelist: Vec<String>,
    pub network_smoothing: String,
    pub theme: String,
    pub history_retention_days: u32,
    pub history_timezone: String,
    pub overlay_visible: bool,
    pub overlay_position: [i32; 2],
    pub overlay_opacity: f64,
    pub overlay_click_through: bool,
    pub overlay_always_on_top: bool,
    pub overlay_skin: String,
    pub overlay_units: String,
    pub overlay_items: Vec<UiItem>,
    pub alerts_rules: Vec<UiAlertRule>,
    pub plugins_enabled: bool,
    pub process_traffic_backend: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        // 与 trafficview_core::config::Config::default() 保持一致。
        Self {
            sample_interval_ms: 1000,
            active_interface: String::new(),
            memory_include_cache: false,
            interface_blacklist: vec![
                "lo".into(),
                "docker*".into(),
                "veth*".into(),
                "br-*".into(),
                "virbr*".into(),
                "tun*".into(),
            ],
            interface_whitelist: Vec::new(),
            network_smoothing: "none".into(),
            theme: "dark".into(),
            history_retention_days: 90,
            history_timezone: String::new(),
            overlay_visible: true,
            overlay_position: [40, 40],
            overlay_opacity: 0.9,
            overlay_click_through: false,
            overlay_always_on_top: true,
            overlay_skin: String::new(),
            overlay_units: "bytes".into(),
            overlay_items: default_items(),
            alerts_rules: Vec::new(),
            plugins_enabled: true,
            process_traffic_backend: "off".into(),
        }
    }
}

fn default_items() -> Vec<UiItem> {
    vec![
        UiItem {
            id: "net_up".into(),
            label: "↑".into(),
            unit: "B/s".into(),
            format: "{value}".into(),
            color: "#7FD1FF".into(),
            visible: true,
        },
        UiItem {
            id: "net_down".into(),
            label: "↓".into(),
            unit: "B/s".into(),
            format: "{value}".into(),
            color: "#8CE99A".into(),
            visible: true,
        },
        UiItem {
            id: "cpu".into(),
            label: "CPU".into(),
            unit: "%".into(),
            format: "{value}".into(),
            color: "#FFD43B".into(),
            visible: true,
        },
        UiItem {
            id: "mem".into(),
            label: "MEM".into(),
            unit: "%".into(),
            format: "{value}".into(),
            color: "#FFA8A8".into(),
            visible: true,
        },
    ]
}

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).expect("constructed D-Bus values are owned")
}

fn as_u32(value: &OwnedValue) -> Option<u32> {
    u32::try_from(value.try_clone().ok()?).ok()
}

fn as_f64(value: &OwnedValue) -> Option<f64> {
    f64::try_from(value.try_clone().ok()?).ok()
}

fn as_bool(value: &OwnedValue) -> Option<bool> {
    bool::try_from(value.try_clone().ok()?).ok()
}

fn as_string(value: &OwnedValue) -> Option<String> {
    String::try_from(value.try_clone().ok()?).ok()
}

fn as_strings(value: &OwnedValue) -> Option<Vec<String>> {
    Vec::<String>::try_from(value.try_clone().ok()?).ok()
}

fn as_map(value: &OwnedValue) -> Option<HashMap<String, OwnedValue>> {
    HashMap::<String, OwnedValue>::try_from(value.try_clone().ok()?).ok()
}

fn as_i32_vec(value: &OwnedValue) -> Option<Vec<i32>> {
    Vec::<i32>::try_from(value.try_clone().ok()?).ok()
}

/// 解析 daemon `GetConfig` 的返回字典；缺失或类型不符的字段回退到默认值。
pub fn from_dbus(dict: &HashMap<String, OwnedValue>) -> UiConfig {
    let mut cfg = UiConfig::default();
    if let Some(v) = dict.get("sample_interval_ms").and_then(as_u32) {
        cfg.sample_interval_ms = v;
    }
    if let Some(v) = dict.get("active_interface").and_then(as_string) {
        cfg.active_interface = v;
    }
    if let Some(v) = dict.get("memory_include_cache").and_then(as_bool) {
        cfg.memory_include_cache = v;
    }
    if let Some(v) = dict.get("interface_blacklist").and_then(as_strings) {
        cfg.interface_blacklist = v;
    }
    if let Some(v) = dict.get("interface_whitelist").and_then(as_strings) {
        cfg.interface_whitelist = v;
    }
    if let Some(v) = dict.get("network_smoothing").and_then(as_string) {
        cfg.network_smoothing = v;
    }
    if let Some(v) = dict.get("theme").and_then(as_string) {
        cfg.theme = v;
    }
    if let Some(history) = dict.get("history").and_then(as_map) {
        if let Some(v) = history.get("retention_days").and_then(as_u32) {
            cfg.history_retention_days = v;
        }
        if let Some(v) = history.get("timezone").and_then(as_string) {
            cfg.history_timezone = v;
        }
    }
    if let Some(alerts) = dict.get("alerts").and_then(as_map) {
        if let Some(rules) = alerts.get("rules").and_then(|v| {
            let entries = Vec::<OwnedValue>::try_from(v.try_clone().ok()?).ok()?;
            let mut out = Vec::with_capacity(entries.len());
            for entry in entries {
                let map = HashMap::<String, OwnedValue>::try_from(entry).ok()?;
                out.push(parse_alert_rule(&map));
            }
            Some(out)
        }) {
            cfg.alerts_rules = rules;
        }
    }
    if let Some(overlay) = dict.get("overlay").and_then(as_map) {
        if let Some(v) = overlay.get("visible").and_then(as_bool) {
            cfg.overlay_visible = v;
        }
        if let Some(v) = overlay.get("position").and_then(as_i32_vec) {
            if let [x, y] = v.as_slice() {
                cfg.overlay_position = [*x, *y];
            }
        }
        if let Some(v) = overlay.get("opacity").and_then(as_f64) {
            cfg.overlay_opacity = v;
        }
        if let Some(v) = overlay.get("click_through").and_then(as_bool) {
            cfg.overlay_click_through = v;
        }
        if let Some(v) = overlay.get("always_on_top").and_then(as_bool) {
            cfg.overlay_always_on_top = v;
        }
        if let Some(v) = overlay.get("skin").and_then(as_string) {
            cfg.overlay_skin = v;
        }
        if let Some(v) = overlay.get("units").and_then(as_string) {
            cfg.overlay_units = v;
        }
        if let Some(items) = overlay.get("items").and_then(|v| {
            let entries = Vec::<OwnedValue>::try_from(v.try_clone().ok()?).ok()?;
            let mut out = Vec::with_capacity(entries.len());
            for entry in entries {
                let map = HashMap::<String, OwnedValue>::try_from(entry).ok()?;
                out.push(parse_item(&map));
            }
            Some(out)
        }) {
            cfg.overlay_items = items;
        }
    }
    if let Some(plugins) = dict.get("plugins").and_then(as_map) {
        if let Some(v) = plugins.get("enabled").and_then(as_bool) {
            cfg.plugins_enabled = v;
        }
    }
    if let Some(pt) = dict.get("process_traffic").and_then(as_map) {
        if let Some(v) = pt.get("backend").and_then(as_string) {
            cfg.process_traffic_backend = v;
        }
    }
    cfg
}

fn parse_alert_rule(map: &HashMap<String, OwnedValue>) -> UiAlertRule {
    let mut rule = UiAlertRule {
        enabled: false,
        metric: "cpu_pct".into(),
        op: ">".into(),
        threshold: 90.0,
        duration_s: 10,
        cooldown_s: 300,
        quiet_hours: String::new(),
        level: "warning".into(),
    };
    if let Some(v) = map.get("enabled").and_then(as_bool) {
        rule.enabled = v;
    }
    if let Some(v) = map.get("metric").and_then(as_string) {
        rule.metric = v;
    }
    if let Some(v) = map.get("op").and_then(as_string) {
        rule.op = v;
    }
    if let Some(v) = map.get("threshold").and_then(as_f64) {
        rule.threshold = v;
    }
    if let Some(v) = map.get("duration_s").and_then(as_u32) {
        rule.duration_s = v;
    }
    if let Some(v) = map.get("cooldown_s").and_then(as_u32) {
        rule.cooldown_s = v;
    }
    if let Some(v) = map.get("quiet_hours").and_then(as_string) {
        rule.quiet_hours = v;
    }
    if let Some(v) = map.get("level").and_then(as_string) {
        rule.level = v;
    }
    rule
}

fn parse_item(map: &HashMap<String, OwnedValue>) -> UiItem {
    let mut item = UiItem {
        id: String::new(),
        label: String::new(),
        unit: String::new(),
        format: "{value}".into(),
        color: "#FFFFFF".into(),
        visible: true,
    };
    if let Some(v) = map.get("id").and_then(as_string) {
        item.id = v;
    }
    if let Some(v) = map.get("label").and_then(as_string) {
        item.label = v;
    }
    if let Some(v) = map.get("unit").and_then(as_string) {
        item.unit = v;
    }
    if let Some(v) = map.get("format").and_then(as_string) {
        item.format = v;
    }
    if let Some(v) = map.get("color").and_then(as_string) {
        item.color = v;
    }
    if let Some(v) = map.get("visible").and_then(as_bool) {
        item.visible = v;
    }
    item
}

fn item_to_value(item: &UiItem) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("id".into(), Value::from(item.id.clone()));
    map.insert("label".into(), Value::from(item.label.clone()));
    map.insert("unit".into(), Value::from(item.unit.clone()));
    map.insert("format".into(), Value::from(item.format.clone()));
    map.insert("color".into(), Value::from(item.color.clone()));
    map.insert("visible".into(), Value::from(item.visible));
    Value::from(map)
}

fn rule_to_value(rule: &UiAlertRule) -> Value<'static> {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("enabled".into(), Value::from(rule.enabled));
    map.insert("metric".into(), Value::from(rule.metric.clone()));
    map.insert("op".into(), Value::from(rule.op.clone()));
    map.insert("threshold".into(), Value::from(rule.threshold));
    map.insert("duration_s".into(), Value::from(rule.duration_s));
    map.insert("cooldown_s".into(), Value::from(rule.cooldown_s));
    map.insert("quiet_hours".into(), Value::from(rule.quiet_hours.clone()));
    map.insert("level".into(), Value::from(rule.level.clone()));
    Value::from(map)
}

/// 组装一次 `UpdateConfig(a{sv})` 的完整请求体；保存按钮一次性提交。
pub fn to_update_dbus(cfg: &UiConfig) -> HashMap<String, OwnedValue> {
    let mut top: HashMap<String, OwnedValue> = HashMap::new();
    top.insert(
        "sample_interval_ms".into(),
        owned(Value::from(cfg.sample_interval_ms)),
    );
    top.insert(
        "active_interface".into(),
        owned(Value::from(cfg.active_interface.clone())),
    );
    top.insert(
        "memory_include_cache".into(),
        owned(Value::from(cfg.memory_include_cache)),
    );
    top.insert(
        "interface_blacklist".into(),
        owned(Value::from(cfg.interface_blacklist.clone())),
    );
    top.insert(
        "interface_whitelist".into(),
        owned(Value::from(cfg.interface_whitelist.clone())),
    );
    top.insert(
        "network_smoothing".into(),
        owned(Value::from(cfg.network_smoothing.clone())),
    );
    top.insert("theme".into(), owned(Value::from(cfg.theme.clone())));

    let mut history: HashMap<String, Value<'static>> = HashMap::new();
    history.insert(
        "retention_days".into(),
        Value::from(cfg.history_retention_days),
    );
    history.insert("timezone".into(), Value::from(cfg.history_timezone.clone()));
    top.insert("history".into(), owned(Value::from(history)));

    let rules: Vec<Value<'static>> = cfg.alerts_rules.iter().map(rule_to_value).collect();
    let mut alerts: HashMap<String, Value<'static>> = HashMap::new();
    alerts.insert("rules".into(), Value::from(rules));
    top.insert("alerts".into(), owned(Value::from(alerts)));

    let items: Vec<Value<'static>> = cfg.overlay_items.iter().map(item_to_value).collect();
    let mut overlay: HashMap<String, Value<'static>> = HashMap::new();
    overlay.insert("visible".into(), Value::from(cfg.overlay_visible));
    overlay.insert(
        "position".into(),
        Value::from(vec![cfg.overlay_position[0], cfg.overlay_position[1]]),
    );
    overlay.insert("opacity".into(), Value::from(cfg.overlay_opacity));
    overlay.insert(
        "click_through".into(),
        Value::from(cfg.overlay_click_through),
    );
    overlay.insert(
        "always_on_top".into(),
        Value::from(cfg.overlay_always_on_top),
    );
    overlay.insert("skin".into(), Value::from(cfg.overlay_skin.clone()));
    overlay.insert("units".into(), Value::from(cfg.overlay_units.clone()));
    overlay.insert("items".into(), Value::from(items));
    top.insert("overlay".into(), owned(Value::from(overlay)));

    let mut plugins: HashMap<String, Value<'static>> = HashMap::new();
    plugins.insert("enabled".into(), Value::from(cfg.plugins_enabled));
    top.insert("plugins".into(), owned(Value::from(plugins)));

    let mut pt: HashMap<String, Value<'static>> = HashMap::new();
    pt.insert(
        "backend".into(),
        Value::from(cfg.process_traffic_backend.clone()),
    );
    top.insert("process_traffic".into(), owned(Value::from(pt)));

    top
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个与 daemon `config_to_dbus(Config::default())` 等价的字典，
    /// 验证设置界面解析结果与默认值一致。
    fn default_dbus_dict() -> HashMap<String, OwnedValue> {
        to_update_dbus(&UiConfig::default())
    }

    #[test]
    fn default_dict_round_trips_to_default_config() {
        let dict = default_dbus_dict();
        let parsed = from_dbus(&dict);
        assert_eq!(parsed, UiConfig::default());
    }

    #[test]
    fn to_update_then_parse_is_stable() {
        let mut cfg = UiConfig {
            sample_interval_ms: 2500,
            theme: "dark".into(),
            active_interface: "eth0".into(),
            interface_whitelist: vec!["eth0".into(), "wlan0".into()],
            network_smoothing: "ema".into(),
            history_retention_days: 30,
            history_timezone: "+08:00".into(),
            overlay_opacity: 0.5,
            overlay_position: [100, 200],
            overlay_units: "bits".into(),
            overlay_visible: false,
            plugins_enabled: false,
            process_traffic_backend: "af_packet".into(),
            memory_include_cache: true,
            ..UiConfig::default()
        };
        cfg.overlay_items[0].visible = false;
        cfg.alerts_rules.push(UiAlertRule {
            enabled: true,
            metric: "cpu_pct".into(),
            op: ">=".into(),
            threshold: 80.0,
            duration_s: 5,
            cooldown_s: 60,
            quiet_hours: "23:00-07:00".into(),
            level: "critical".into(),
        });

        let dict = to_update_dbus(&cfg);
        let parsed = from_dbus(&dict);
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        // 空字典应解析为默认配置，不 panic。
        let empty: HashMap<String, OwnedValue> = HashMap::new();
        let parsed = from_dbus(&empty);
        assert_eq!(parsed, UiConfig::default());
    }

    #[test]
    fn wrong_type_keys_fall_back_to_defaults() {
        let mut dict = default_dbus_dict();
        // sample_interval_ms 应为 u32，故意塞字符串，应回退默认 1000。
        dict.insert(
            "sample_interval_ms".into(),
            owned(Value::from("not-a-number")),
        );
        let parsed = from_dbus(&dict);
        assert_eq!(parsed.sample_interval_ms, 1000);
        // 其余字段仍正确。
        assert_eq!(parsed.theme, "dark");
    }

    #[test]
    fn position_with_wrong_length_is_ignored() {
        let mut dict = default_dbus_dict();
        let mut overlay: HashMap<String, OwnedValue> = HashMap::new();
        overlay.insert("position".into(), owned(Value::from(vec![1, 2, 3])));
        dict.insert("overlay".into(), owned(Value::from(overlay)));
        let parsed = from_dbus(&dict);
        // 长度不对时回退默认位置 [40,40]。
        assert_eq!(parsed.overlay_position, [40, 40]);
    }
}
