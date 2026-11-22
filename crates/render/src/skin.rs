//! 原生 `skin.toml` 皮肤模型：解析、资源边界校验与逐项告警。
//!
//! 坐标使用逻辑像素；渲染时再按 DPI/缩放因子转换。背景资源必须位于皮肤目录内，
//! 不接受绝对路径或 `..` 逃逸。

use std::{
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;

use trafficview_core::config::RateUnit;

use crate::value::is_known_id;

pub const DEFAULT_COLOR: &str = "#FFFFFF";
pub const DEFAULT_FONT_SIZE: f32 = 14.0;

fn default_format() -> String {
    "{value}".into()
}

/// 皮肤根模型。`parse` 只做语法与几何校验；`load` 额外校验背景文件存在且可解码。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skin {
    pub meta: SkinMeta,
    pub canvas: Canvas,
    #[serde(default)]
    pub font: Font,
    #[serde(default, rename = "item")]
    pub items: Vec<SkinItem>,
    #[serde(skip)]
    pub warnings: Vec<String>,
    #[serde(skip)]
    pub content_hash: u64,
    #[serde(skip)]
    pub base_dir: PathBuf,
    #[serde(skip)]
    pub background_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkinMeta {
    pub name: String,
    pub author: String,
    pub version: String,
}

impl Default for SkinMeta {
    fn default() -> Self {
        Self {
            name: "Untitled".into(),
            author: String::new(),
            version: "1".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub background: Option<String>,
    pub background_color: Option<String>,
    /// `[left, top, right, bottom]` 九宫格边距。
    pub nine_patch: Option<[u32; 4]>,
    pub opacity: f32,
    pub units: RateUnit,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            width: 240,
            height: 80,
            background: None,
            background_color: None,
            nine_patch: None,
            opacity: 1.0,
            units: RateUnit::Bytes,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    pub family: String,
    pub size: f32,
    pub color: String,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: "Sans".into(),
            size: DEFAULT_FONT_SIZE,
            color: DEFAULT_COLOR.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    #[default]
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Threshold {
    pub at: f64,
    pub color: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkinItem {
    pub id: String,
    /// `[x, y, width, height]`，逻辑像素。
    pub rect: [i64; 4],
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_format")]
    pub format: String,
    /// 缺省时继承 `[font].color`。
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub align: Align,
    #[serde(default)]
    pub mono: bool,
    #[serde(default)]
    pub font_size: Option<f32>,
    /// 升序阈值；命中值 `>= at` 时取该颜色。
    #[serde(default)]
    pub thresholds: Vec<Threshold>,
}

impl SkinItem {
    /// 阈值颜色：基础色 → 依次命中不高于当前值的最高阈值。
    pub fn color_for(&self, value: f64) -> String {
        let mut color = self.color.clone();
        for threshold in &self.thresholds {
            if value >= threshold.at {
                color = threshold.color.clone();
            } else {
                break;
            }
        }
        color
    }

    #[doc(hidden)]
    pub fn threshold_test(pairs: &[(f64, &str)]) -> Self {
        Self {
            id: "cpu".into(),
            rect: [0, 0, 100, 20],
            label: String::new(),
            format: default_format(),
            color: DEFAULT_COLOR.into(),
            align: Align::Left,
            mono: false,
            font_size: None,
            thresholds: pairs
                .iter()
                .map(|(at, color)| Threshold {
                    at: *at,
                    color: (*color).into(),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub enum SkinError {
    Io { path: PathBuf, source: io::Error },
    Toml(toml::de::Error),
    Invalid(String),
}

impl fmt::Display for SkinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Toml(error) => write!(f, "invalid skin TOML: {error}"),
            Self::Invalid(message) => write!(f, "invalid skin: {message}"),
        }
    }
}

impl std::error::Error for SkinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Toml(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<toml::de::Error> for SkinError {
    fn from(error: toml::de::Error) -> Self {
        Self::Toml(error)
    }
}

impl Skin {
    /// 从皮肤目录读取 `skin.toml`，并校验背景资源。
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, SkinError> {
        let directory = directory.as_ref();
        let path = directory.join("skin.toml");
        let source = fs::read_to_string(&path).map_err(|source| SkinError::Io {
            path: path.clone(),
            source,
        })?;
        let mut skin = Self::parse(&source, directory)?;
        if let Some(background) = skin.canvas.background.clone() {
            let resolved = skin.background_path.clone().ok_or_else(|| {
                SkinError::Invalid(format!(
                    "background '{background}' is outside skin directory"
                ))
            })?;
            let bytes = fs::read(&resolved).map_err(|error| {
                SkinError::Invalid(format!(
                    "cannot read background '{}': {error}",
                    resolved.display()
                ))
            })?;
            let pixmap = tiny_skia::Pixmap::decode_png(&bytes).map_err(|error| {
                SkinError::Invalid(format!(
                    "background '{}' is not a valid PNG: {error}",
                    resolved.display()
                ))
            })?;
            if let Some(insets) = skin.canvas.nine_patch {
                crate::nine_patch::validate_insets(pixmap.width(), pixmap.height(), insets)
                    .map_err(SkinError::Invalid)?;
            }
            skin.content_hash = fnv1a(&bytes, skin.content_hash);
            skin.background_path = Some(resolved);
        }
        Ok(skin)
    }

    /// 纯文本解析：不访问文件系统，背景路径仅做词法边界校验。
    pub fn parse(source: &str, base_dir: impl AsRef<Path>) -> Result<Self, SkinError> {
        let mut skin: Self = toml::from_str(source)?;
        skin.base_dir = base_dir.as_ref().to_path_buf();
        skin.content_hash = fnv1a(source.as_bytes(), FNV_OFFSET);

        if skin.canvas.width == 0 || skin.canvas.height == 0 {
            return Err(SkinError::Invalid(
                "canvas width and height must be greater than zero".into(),
            ));
        }
        if !skin.canvas.opacity.is_finite() || !(0.0..=1.0).contains(&skin.canvas.opacity) {
            return Err(SkinError::Invalid(
                "canvas opacity must be finite within 0..=1".into(),
            ));
        }
        if let Some(background) = skin.canvas.background.as_deref() {
            skin.background_path = Some(resolve_asset(&skin.base_dir, background)?);
        }
        if let Some(color) = skin.canvas.background_color.as_deref() {
            if !is_valid_color(color) {
                return Err(SkinError::Invalid(format!(
                    "canvas background_color '{color}' is not #RRGGBB or #RRGGBBAA"
                )));
            }
        }
        if !skin.font.size.is_finite() || skin.font.size <= 0.0 {
            return Err(SkinError::Invalid(
                "font size must be a positive number".into(),
            ));
        }
        if !is_valid_color(&skin.font.color) {
            return Err(SkinError::Invalid(format!(
                "font color '{}' is not #RRGGBB or #RRGGBBAA",
                skin.font.color
            )));
        }
        if let Some(insets) = skin.canvas.nine_patch {
            if insets[0].saturating_add(insets[2]) >= skin.canvas.width
                || insets[1].saturating_add(insets[3]) >= skin.canvas.height
            {
                return Err(SkinError::Invalid(
                    "nine_patch insets must leave a stretchable center".into(),
                ));
            }
        }
        if skin.items.is_empty() {
            return Err(SkinError::Invalid(
                "skin must declare at least one [[item]]".into(),
            ));
        }

        let mut seen = std::collections::HashSet::new();
        for item in &mut skin.items {
            if item.id.trim().is_empty() {
                return Err(SkinError::Invalid("item id must not be empty".into()));
            }
            if !seen.insert(item.id.clone()) {
                return Err(SkinError::Invalid(format!(
                    "duplicate item id: {}",
                    item.id
                )));
            }
            let [x, y, width, height] = item.rect;
            let (x, y) = (u32::try_from(x), u32::try_from(y));
            let (width, height) = (u32::try_from(width), u32::try_from(height));
            let (Ok(x), Ok(y), Ok(width), Ok(height)) = (x, y, width, height) else {
                return Err(SkinError::Invalid(format!(
                    "item '{}' rect must use non-negative x/y and positive width/height",
                    item.id
                )));
            };
            if width == 0 || height == 0 {
                return Err(SkinError::Invalid(format!(
                    "item '{}' width and height must be greater than zero",
                    item.id
                )));
            }
            if x.saturating_add(width) > skin.canvas.width
                || y.saturating_add(height) > skin.canvas.height
            {
                return Err(SkinError::Invalid(format!(
                    "item '{}' rect exceeds canvas bounds",
                    item.id
                )));
            }
            if let Some(size) = item.font_size {
                if !size.is_finite() || size <= 0.0 {
                    return Err(SkinError::Invalid(format!(
                        "item '{}' font_size must be a positive number",
                        item.id
                    )));
                }
            }
            if item.color.is_empty() {
                item.color = skin.font.color.clone();
            } else if !is_valid_color(&item.color) {
                return Err(SkinError::Invalid(format!(
                    "item '{}' color '{}' is not #RRGGBB or #RRGGBBAA",
                    item.id, item.color
                )));
            }
            let mut previous = f64::NEG_INFINITY;
            for threshold in &item.thresholds {
                if !threshold.at.is_finite() || !(0.0..=100.0).contains(&threshold.at) {
                    return Err(SkinError::Invalid(format!(
                        "item '{}' threshold 'at' must be within 0..=100",
                        item.id
                    )));
                }
                if threshold.at <= previous {
                    return Err(SkinError::Invalid(format!(
                        "item '{}' thresholds must be sorted ascending by 'at'",
                        item.id
                    )));
                }
                if !is_valid_color(&threshold.color) {
                    return Err(SkinError::Invalid(format!(
                        "item '{}' threshold color '{}' is not #RRGGBB or #RRGGBBAA",
                        item.id, threshold.color
                    )));
                }
                previous = threshold.at;
            }
            if !is_known_id(&item.id) {
                skin.warnings.push(format!(
                    "unknown item id '{}' (known: net_up, net_down, cpu, mem, plugin.<id>)",
                    item.id
                ));
            }
        }
        Ok(skin)
    }

    pub fn font_size_for(&self, item: &SkinItem) -> f32 {
        item.font_size.unwrap_or(self.font.size)
    }
}

/// 校验 `#RRGGBB` / `#RRGGBBAA`。
pub fn is_valid_color(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 6 | 8) && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 词法解析资源路径：拒绝绝对路径与逃逸皮肤目录的 `..`。
fn resolve_asset(base: &Path, value: &str) -> Result<PathBuf, SkinError> {
    let path = Path::new(value);
    let outside = || SkinError::Invalid(format!("background '{value}' is outside skin directory"));
    if path.is_absolute() {
        return Err(outside());
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return Err(outside());
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(outside()),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(outside());
    }
    Ok(base.join(clean))
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
