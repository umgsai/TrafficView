//! v1 配置模型：默认值、校验与 TOML 序列化。
//!
//! 字段一旦发布只允许新增（带 serde 默认值），不重命名、不改单位。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// 采样间隔允许范围（毫秒）。
pub const SAMPLE_INTERVAL_MIN_MS: u32 = 500;
pub const SAMPLE_INTERVAL_MAX_MS: u32 = 30_000;

/// 悬浮窗透明度允许范围。
pub const OVERLAY_OPACITY_MIN: f32 = 0.1;
pub const OVERLAY_OPACITY_MAX: f32 = 1.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sample_interval_ms: u32,
    /// 空字符串表示自动选择默认路由网卡。
    pub active_interface: String,
    /// false 使用 `MemTotal - MemAvailable`，true 使用 `MemTotal - MemFree`。
    pub memory_include_cache: bool,
    pub interface_blacklist: Vec<String>,
    pub overlay: OverlayConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayItem {
    pub id: String,
    pub label: String,
    pub unit: String,
    /// 模板，`{value}` 会被替换为格式化后的数值。
    pub format: String,
    pub color: String,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub visible: bool,
    pub position: [i32; 2],
    pub opacity: f32,
    pub click_through: bool,
    pub always_on_top: bool,
    pub items: Vec<DisplayItem>,
}

impl Default for Config {
    fn default() -> Self {
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
            overlay: OverlayConfig::default(),
        }
    }
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            visible: true,
            position: [40, 40],
            opacity: 0.9,
            click_through: false,
            always_on_top: true,
            items: default_items(),
        }
    }
}

impl Default for DisplayItem {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            unit: String::new(),
            format: "{value}".into(),
            color: "#FFFFFF".into(),
            visible: true,
        }
    }
}

fn default_items() -> Vec<DisplayItem> {
    vec![
        DisplayItem {
            id: "net_up".into(),
            label: "↑".into(),
            unit: "B/s".into(),
            format: "{value}".into(),
            color: "#7FD1FF".into(),
            visible: true,
        },
        DisplayItem {
            id: "net_down".into(),
            label: "↓".into(),
            unit: "B/s".into(),
            format: "{value}".into(),
            color: "#8CE99A".into(),
            visible: true,
        },
        DisplayItem {
            id: "cpu".into(),
            label: "CPU".into(),
            unit: "%".into(),
            format: "{value}".into(),
            color: "#FFD43B".into(),
            visible: true,
        },
        DisplayItem {
            id: "mem".into(),
            label: "MEM".into(),
            unit: "%".into(),
            format: "{value}".into(),
            color: "#FFA8A8".into(),
            visible: true,
        },
    ]
}

impl Config {
    /// 校验配置边界；错误信息可直接展示给用户。
    pub fn validate(&self) -> Result<(), String> {
        if !(SAMPLE_INTERVAL_MIN_MS..=SAMPLE_INTERVAL_MAX_MS).contains(&self.sample_interval_ms) {
            return Err(format!(
                "sample_interval_ms must be within {SAMPLE_INTERVAL_MIN_MS}..={SAMPLE_INTERVAL_MAX_MS}"
            ));
        }
        if !self.overlay.opacity.is_finite()
            || !(OVERLAY_OPACITY_MIN..=OVERLAY_OPACITY_MAX).contains(&self.overlay.opacity)
        {
            return Err(format!(
                "overlay.opacity must be finite within {OVERLAY_OPACITY_MIN}..={OVERLAY_OPACITY_MAX}"
            ));
        }
        if self.overlay.items.is_empty() {
            return Err("overlay.items must not be empty".into());
        }
        let mut seen = HashSet::new();
        for item in &self.overlay.items {
            if item.id.trim().is_empty() {
                return Err("overlay item id must not be empty".into());
            }
            if !seen.insert(item.id.as_str()) {
                return Err(format!("duplicate overlay item id: {}", item.id));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_mvp_contract() {
        let cfg = Config::default();
        assert_eq!(cfg.sample_interval_ms, 1000);
        assert_eq!(cfg.active_interface, "");
        assert!(cfg.overlay.visible);
        assert_eq!(cfg.overlay.opacity, 0.9);
    }

    #[test]
    fn rejects_invalid_values() {
        let mut cfg = Config::default();
        cfg.sample_interval_ms = 100;
        assert!(cfg.validate().is_err());
        cfg = Config::default();
        cfg.overlay.opacity = 1.1;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_or_empty_item_ids() {
        let mut cfg = Config::default();
        cfg.overlay.items[1].id = cfg.overlay.items[0].id.clone();
        assert!(cfg.validate().is_err());
        cfg = Config::default();
        cfg.overlay.items[0].id = "  ".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn toml_round_trip_preserves_display_items() {
        let cfg = Config::default();
        let decoded: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(decoded, cfg);
        assert_eq!(
            decoded
                .overlay
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["net_up", "net_down", "cpu", "mem"]
        );
    }
}
