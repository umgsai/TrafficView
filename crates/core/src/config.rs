//! v1 配置模型：默认值、校验与 TOML 序列化。
//!
//! 字段一旦发布只允许新增（带 serde 默认值），不重命名、不改单位。

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fmt, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

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
    /// 优先选择的白名单；命中且接口存在时高于默认路由规则。
    pub interface_whitelist: Vec<String>,
    /// 展示速率平滑；历史累计始终使用原始计数器。
    pub network_smoothing: NetworkSmoothing,
    /// `auto` 跟随桌面 portal，`light`/`dark` 强制指定。
    pub theme: ThemeMode,
    pub history: HistoryConfig,
    pub alerts: AlertsConfig,
    pub overlay: OverlayConfig,
    pub plugins: PluginsConfig,
    pub process_traffic: ProcessTrafficConfig,
}

/// 展示速率平滑算法；默认不改变原始速率。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkSmoothing {
    #[default]
    None,
    MovingAverage3,
    Ema,
}

/// 深色/浅色主题选择。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// 分钟明细保留天数；日汇总不受清理影响。
    pub retention_days: u32,
    /// 空字符串表示系统本地时区；也接受 `UTC` 或 `+08:00`。
    pub timezone: String,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            retention_days: 90,
            timezone: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    /// 默认不启用任何规则，避免升级后突然打扰用户。
    pub rules: Vec<AlertRule>,
}

/// 子进程插件开关；插件清单位于 XDG data 目录，不在 config.toml 中声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    pub enabled: bool,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 进程级流量后端；默认 off，不连接 system bus、不申请任何权限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessTrafficBackend {
    #[default]
    Off,
    Ebpf,
    AfPacket,
    Auto,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessTrafficConfig {
    pub backend: ProcessTrafficBackend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertRule {
    pub enabled: bool,
    /// `cpu_pct`、`mem_pct`、`net_rx_bps`、`net_tx_bps`、`month_bytes`。
    pub metric: String,
    /// `>`、`>=`、`<`、`<=`。
    pub op: String,
    pub threshold: f64,
    /// 连续满足条件的秒数。
    pub duration_s: u32,
    /// 恢复后再次通知的最短间隔秒数。
    pub cooldown_s: u32,
    /// 形如 `23:00-07:00`，可跨午夜；空表示不静默。
    pub quiet_hours: String,
    /// `info`、`warning`、`critical`。
    pub level: String,
}

impl Default for AlertRule {
    fn default() -> Self {
        Self {
            enabled: false,
            metric: "cpu_pct".into(),
            op: ">".into(),
            threshold: 90.0,
            duration_s: 10,
            cooldown_s: 300,
            quiet_hours: String::new(),
            level: "warning".into(),
        }
    }
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

/// 速率显示单位。默认字节每秒，保持 M1 行为。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateUnit {
    #[default]
    Bytes,
    Bits,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionPreset {
    #[default]
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub visible: bool,
    pub position: [i32; 2],
    #[serde(default)]
    pub position_preset: PositionPreset,
    pub opacity: f32,
    pub click_through: bool,
    pub always_on_top: bool,
    /// 切换为普通窗口（不设 override_redirect），使窗口管理器将其纳入任务栏。
    pub show_in_taskbar: bool,
    /// 原生皮肤目录；空字符串使用内置固定布局。
    pub skin: String,
    pub units: RateUnit,
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
            interface_whitelist: Vec::new(),
            network_smoothing: NetworkSmoothing::None,
            theme: ThemeMode::Auto,
            history: HistoryConfig::default(),
            alerts: AlertsConfig::default(),
            overlay: OverlayConfig::default(),
            plugins: PluginsConfig::default(),
            process_traffic: ProcessTrafficConfig::default(),
        }
    }
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            visible: true,
            position: [40, 40],
            position_preset: PositionPreset::BottomRight,
            opacity: 0.7,
            click_through: false,
            always_on_top: true,
            show_in_taskbar: false,
            skin: String::new(),
            units: RateUnit::Bytes,
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
        if !(1..=3650).contains(&self.history.retention_days) {
            return Err("history.retention_days must be within 1..=3650".into());
        }
        if !self.history.timezone.is_empty()
            && self.history.timezone != "UTC"
            && parse_offset_minutes(&self.history.timezone).is_none()
        {
            return Err(format!(
                "history.timezone '{}' must be empty, UTC or +HH:MM/-HH:MM",
                self.history.timezone
            ));
        }
        for rule in &self.alerts.rules {
            if !matches!(
                rule.metric.as_str(),
                "cpu_pct" | "mem_pct" | "net_rx_bps" | "net_tx_bps" | "month_bytes"
            ) {
                return Err(format!(
                    "alerts rule metric '{}' is not supported",
                    rule.metric
                ));
            }
            if !matches!(rule.op.as_str(), ">" | ">=" | "<" | "<=") {
                return Err(format!("alerts rule op '{}' is not supported", rule.op));
            }
            if !rule.threshold.is_finite() || rule.threshold < 0.0 {
                return Err("alerts rule threshold must be a non-negative number".into());
            }
            if !matches!(rule.level.as_str(), "info" | "warning" | "critical") {
                return Err(format!(
                    "alerts rule level '{}' is not supported",
                    rule.level
                ));
            }
            if !rule.quiet_hours.is_empty() && !valid_quiet_hours(&rule.quiet_hours) {
                return Err(format!(
                    "alerts rule quiet_hours '{}' must be HH:MM-HH:MM",
                    rule.quiet_hours
                ));
            }
        }
        Ok(())
    }
}

/// 解析 `+08:00` / `-05:30` 为分钟偏移；不支持 `UTC` 字样。
pub fn parse_offset_minutes(value: &str) -> Option<i32> {
    let (sign, rest) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => return None,
    };
    let (hours, minutes) = rest.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

/// 校验 `HH:MM-HH:MM`；允许跨午夜。
pub fn valid_quiet_hours(value: &str) -> bool {
    let Some((start, end)) = value.split_once('-') else {
        return false;
    };
    parse_hhmm(start).is_some() && parse_hhmm(end).is_some()
}

/// 解析 `HH:MM` 为当日分钟数。
pub fn parse_hhmm(value: &str) -> Option<u32> {
    let (hours, minutes) = value.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u32 = hours.parse().ok()?;
    let minutes: u32 = minutes.parse().ok()?;
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Invalid(String),
    NoConfigDir,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "config I/O error: {error}"),
            Self::Parse(error) => write!(f, "invalid config TOML: {error}"),
            Self::Serialize(error) => write!(f, "cannot encode config: {error}"),
            Self::Invalid(message) => write!(f, "invalid config: {message}"),
            Self::NoConfigDir => write!(f, "neither XDG_CONFIG_HOME nor HOME is set"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::Invalid(_) | Self::NoConfigDir => None,
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        Self::Parse(error)
    }
}

impl From<toml::ser::Error> for ConfigError {
    fn from(error: toml::ser::Error) -> Self {
        Self::Serialize(error)
    }
}

/// XDG 配置文件的读写边界：校验、原子替换、不覆盖损坏文件。
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `$XDG_CONFIG_HOME/trafficview/config.toml`，未设置时 `$HOME/.config/...`。
    pub fn xdg() -> Result<Self, ConfigError> {
        if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
            return Ok(Self::at(
                PathBuf::from(dir).join("trafficview").join("config.toml"),
            ));
        }
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or(ConfigError::NoConfigDir)?;
        Ok(Self::at(
            PathBuf::from(home)
                .join(".config")
                .join("trafficview")
                .join("config.toml"),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(&self.path)?;
        let config: Config = toml::from_str(&text)?;
        config.validate().map_err(ConfigError::Invalid)?;
        Ok(config)
    }

    /// 文件缺失时写入默认配置；已存在但损坏时返回错误且不改动原文件。
    pub fn load_or_create(&self) -> Result<Config, ConfigError> {
        match self.load() {
            Ok(config) => Ok(config),
            Err(ConfigError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                let config = Config::default();
                self.save(&config)?;
                Ok(config)
            }
            Err(error) => Err(error),
        }
    }

    /// 校验 → 同目录临时文件 → fsync → rename → 父目录 fsync。
    pub fn save(&self, config: &Config) -> Result<(), ConfigError> {
        config.validate().map_err(ConfigError::Invalid)?;
        let text = toml::to_string_pretty(config)?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.temp_path();
        let write = (|| -> io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            use io::Write;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })();
        if let Err(error) = write {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
        if let Err(error) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(error.into());
        }
        if let Some(parent) = self.path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    fn temp_path(&self) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".into());
        self.path
            .with_file_name(format!(".{name}.tmp.{}.{nonce}", std::process::id()))
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
        assert_eq!(cfg.overlay.opacity, 0.7);
        assert_eq!(cfg.overlay.units, RateUnit::Bytes);
        assert!(cfg.overlay.skin.is_empty());
        assert_eq!(
            cfg.process_traffic.backend,
            ProcessTrafficBackend::Off,
            "process traffic must stay off by default"
        );
    }

    #[test]
    fn rejects_invalid_values() {
        let cfg = Config {
            sample_interval_ms: 100,
            ..Config::default()
        };
        assert!(cfg.validate().is_err());
        let cfg = Config {
            overlay: OverlayConfig {
                opacity: 1.1,
                ..OverlayConfig::default()
            },
            ..Config::default()
        };
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
