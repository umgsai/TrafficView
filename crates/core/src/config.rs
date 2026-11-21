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
