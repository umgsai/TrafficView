//! 平台无关的固定布局 ARGB 渲染。96 DPI 为基准，输出 BGRA 预乘字节。

pub mod nine_patch;
pub mod skin;
pub mod value;

use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

use cosmic_text::{
    Attrs, Buffer, Color as TextColor, Family, FontSystem, Metrics, Shaping, SwashCache, Weight,
};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use trafficview_core::{
    config::{Config, DisplayItem},
    format::format_rate,
    ColorScheme, Snapshot,
};

pub use skin::{Skin, SkinError};
pub use value::{display_value, is_available, is_known_id, numeric_value};

pub const BASE_DPI: u32 = 96;
const ROW_HEIGHT: f32 = 22.0;
const PADDING: f32 = 8.0;
const GAP: f32 = 8.0;
const BASE_WIDTH: f32 = 200.0;
const FONT_SIZE: f32 = 13.0;
/// 文字与背景的最低对比度；低于该值时保持色相、调整亮度。
const MIN_CONTRAST: f32 = 4.5;

/// 前端从 D-Bus Alert 信号维护的活动告警；只覆盖显示颜色，不修改皮肤或配置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlertState {
    active: BTreeMap<String, ActiveAlert>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveAlert {
    item_id: &'static str,
    color: &'static str,
}

impl AlertState {
    /// 应用一条 Alert 信号，返回显示状态是否发生变化。
    pub fn apply_event(&mut self, level: &str, code: &str, message: &str) -> bool {
        let Some(item_id) = alert_item_id(code) else {
            return false;
        };
        // daemon 的恢复消息当前带有指标标签，兼容“已恢复”位于开头或标签之后。
        if message.contains("已恢复") {
            return self.active.remove(code).is_some();
        }
        let color = match level {
            "info" => "#7FD1FF",
            "warning" => "#FFC107",
            "critical" => "#F44336",
            _ => return false,
        };
        let alert = ActiveAlert { item_id, color };
        if self.active.get(code) == Some(&alert) {
            return false;
        }
        self.active.insert(code.to_owned(), alert);
        true
    }

    pub fn color_for(&self, item_id: &str) -> Option<&'static str> {
        self.active
            .values()
            .find(|alert| alert.item_id == item_id)
            .map(|alert| alert.color)
    }

    fn cache_key(&self) -> String {
        let mut key = String::new();
        for (code, alert) in &self.active {
            key.push_str(code);
            key.push('=');
            key.push_str(alert.color);
            key.push(',');
        }
        key
    }
}

fn alert_item_id(code: &str) -> Option<&'static str> {
    match code {
        "cpu.high" => Some("cpu"),
        "mem.high" => Some("mem"),
        "net.rx.high" => Some("net_down"),
        "net.tx.high" => Some("net_up"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayoutItem {
    pub id: String,
    pub label: String,
    pub text: String,
    pub color: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub items: Vec<LayoutItem>,
    pub scale: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HitRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedFrame {
    pub width: u32,
    pub height: u32,
    /// 预乘 BGRA，可直接用于 X11 32bpp ZPixmap。
    pub argb: Vec<u8>,
    pub cache_key: String,
    pub regions: Vec<HitRegion>,
}

/// 皮肤渲染器。相同显示值 + 相同皮肤 + 相同缩放不会重复栅格化。
pub struct Renderer {
    skin: Skin,
    background: Option<Pixmap>,
    background_light: Option<Pixmap>,
    /// 背景图的平均色，用于判断文字该压暗还是提亮。
    background_rgb: Option<(u8, u8, u8)>,
    background_light_rgb: Option<(u8, u8, u8)>,
    scheme: ColorScheme,
    last_key: Option<String>,
    last_frame: Option<RenderedFrame>,
    rasterize_count: u64,
}

impl Renderer {
    pub fn new(skin: Skin) -> Result<Self, SkinError> {
        let background = load_background(&skin, skin.background_path.as_deref(), "background")?;
        let background_light =
            load_background(&skin, skin.background_light.as_deref(), "background_light")?;
        if skin.canvas.nine_patch.is_some() && background.is_none() && background_light.is_none() {
            return Err(SkinError::Invalid(
                "nine_patch requires a background PNG".into(),
            ));
        }
        let background_rgb = background.as_ref().and_then(average_rgb);
        let background_light_rgb = background_light.as_ref().and_then(average_rgb);
        Ok(Self {
            skin,
            background,
            background_light,
            background_rgb,
            background_light_rgb,
            scheme: ColorScheme::Dark,
            last_key: None,
            last_frame: None,
            rasterize_count: 0,
        })
    }

    pub fn skin(&self) -> &Skin {
        &self.skin
    }

    /// 切换主题只改变背景资源；item 几何保持不变，并强制下一帧重绘。
    pub fn set_color_scheme(&mut self, scheme: ColorScheme) {
        if self.scheme != scheme {
            self.scheme = scheme;
            self.last_key = None;
        }
    }

    pub fn color_scheme(&self) -> ColorScheme {
        self.scheme
    }

    /// 实际栅格化次数；缓存命中不增加。
    pub fn rasterize_count(&self) -> u64 {
        self.rasterize_count
    }

    pub fn render(&mut self, snapshot: &Snapshot, scale: f32) -> Result<RenderedFrame, SkinError> {
        self.render_with_alerts(snapshot, scale, &AlertState::default())
    }

    pub fn render_with_alerts(
        &mut self,
        snapshot: &Snapshot,
        scale: f32,
        alerts: &AlertState,
    ) -> Result<RenderedFrame, SkinError> {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let key = self.cache_key(snapshot, scale, alerts);
        if self.last_key.as_deref() == Some(key.as_str()) {
            if let Some(frame) = &self.last_frame {
                return Ok(frame.clone());
            }
        }
        let frame = self.rasterize(snapshot, scale, alerts)?;
        self.last_key = Some(key);
        self.last_frame = Some(frame.clone());
        self.rasterize_count += 1;
        Ok(frame)
    }

    fn cache_key(&self, snapshot: &Snapshot, scale: f32, alerts: &AlertState) -> String {
        let mut key = format!(
            "{:016x}:{scale:.4}:{:.3}:{}:",
            self.skin.content_hash,
            self.skin.canvas.opacity,
            match self.scheme {
                ColorScheme::Light => "light",
                ColorScheme::Dark => "dark",
            }
        );
        for item in &self.skin.items {
            key.push_str(&item.id);
            key.push('=');
            key.push_str(&value::display_value(
                &item.id,
                snapshot,
                self.skin.canvas.units,
                &item.format,
            ));
            key.push(',');
        }
        key.push_str(&alerts.cache_key());
        key
    }

    fn background_for(&self) -> Option<&Pixmap> {
        match self.scheme {
            ColorScheme::Light => self.background_light.as_ref().or(self.background.as_ref()),
            ColorScheme::Dark => self.background.as_ref(),
        }
    }

    /// 文字所在的背景色：与绘制顺序一致，背景图优先于 `background_color`。
    fn text_background(&self) -> (u8, u8, u8) {
        let image_average = match self.scheme {
            ColorScheme::Light => self.background_light_rgb.or(self.background_rgb),
            ColorScheme::Dark => self.background_rgb,
        };
        if self.background_for().is_some() {
            return image_average.unwrap_or_else(|| scheme_background(self.scheme));
        }
        self.skin
            .canvas
            .background_color
            .as_deref()
            .map(parse_color)
            .unwrap_or_else(|| scheme_background(self.scheme))
    }

    fn rasterize(
        &self,
        snapshot: &Snapshot,
        scale: f32,
        alerts: &AlertState,
    ) -> Result<RenderedFrame, SkinError> {
        let width = ((self.skin.canvas.width as f32) * scale).round().max(1.0) as u32;
        let height = ((self.skin.canvas.height as f32) * scale).round().max(1.0) as u32;
        let mut pixmap = Pixmap::new(width, height)
            .ok_or_else(|| SkinError::Invalid("canvas size must not be zero".into()))?;

        if let Some(background) = self.background_for() {
            let scaled = match self.skin.canvas.nine_patch {
                Some(insets) => nine_patch::scale_nine_patch(background, insets, width, height)
                    .map_err(SkinError::Invalid)?,
                None => nine_patch::resize_nearest(background, width, height),
            };
            pixmap.draw_pixmap(
                0,
                0,
                scaled.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        } else if let Some(color) = &self.skin.canvas.background_color {
            let (r, g, b, a) = parse_color_rgba(color);
            pixmap.fill(Color::from_rgba8(r, g, b, a));
        }

        let mut regions = Vec::with_capacity(self.skin.items.len());
        let background_rgb = self.text_background();
        let opacity = self.skin.canvas.opacity.clamp(0.0, 1.0);
        let mut guard = context().lock().expect("render context poisoned");
        let context = &mut *guard;
        for item in &self.skin.items {
            if !value::is_available(&item.id, snapshot) {
                continue;
            }
            let [x, y, item_width, item_height] = item.rect;
            let x = ((x as f32) * scale).round() as i32;
            let y = ((y as f32) * scale).round() as i32;
            let item_width = ((item_width as f32) * scale).round() as i32;
            let item_height = ((item_height as f32) * scale).round() as i32;
            regions.push(HitRegion {
                x,
                y,
                width: item_width,
                height: item_height,
            });

            let formatted =
                value::display_value(&item.id, snapshot, self.skin.canvas.units, &item.format);
            if formatted.is_empty() {
                continue;
            }
            let text = if item.label.is_empty() {
                formatted
            } else {
                format!("{} {}", item.label, formatted)
            };
            let color = if let Some(color) = alerts.color_for(&item.id) {
                color.to_owned()
            } else {
                match value::numeric_value(&item.id, snapshot) {
                    Some(value) => item.color_for(value),
                    None => item.color.clone(),
                }
            };
            let (r, g, b, a) = parse_color_rgba(&color);
            let color = {
                let (r, g, b) = readable_color((r, g, b), background_rgb, opacity, MIN_CONTRAST);
                (r, g, b, a)
            };
            draw_item_text(
                context,
                &mut pixmap,
                item,
                &text,
                color,
                scale,
                (x, y, item_width, item_height),
                &self.skin,
            );
        }
        drop(guard);

        let factor = (self.skin.canvas.opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
        if factor < 255 {
            for byte in pixmap.data_mut() {
                *byte = ((*byte as u32 * factor + 127) / 255) as u8;
            }
        }

        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for pixel in pixmap.data().as_chunks::<4>().0 {
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        Ok(RenderedFrame {
            width,
            height,
            argb: bgra,
            cache_key: self.cache_key(snapshot, scale, alerts),
            regions,
        })
    }
}

fn load_background(
    skin: &Skin,
    path: Option<&std::path::Path>,
    label: &str,
) -> Result<Option<Pixmap>, SkinError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let pixmap = Pixmap::load_png(path).map_err(|error| {
        SkinError::Invalid(format!(
            "cannot decode {label} '{}': {error}",
            path.display()
        ))
    })?;
    if let Some(insets) = skin.canvas.nine_patch {
        nine_patch::validate_insets(pixmap.width(), pixmap.height(), insets)
            .map_err(SkinError::Invalid)?;
    }
    Ok(Some(pixmap))
}

/// 将 `RenderedFrame`（BGRA 预乘）编码为 PNG 字节，供离屏预览使用。
pub fn encode_png(frame: &RenderedFrame) -> Result<Vec<u8>, String> {
    let mut pixmap = Pixmap::new(frame.width, frame.height)
        .ok_or_else(|| "frame size must not be zero".to_string())?;
    let (source_pixels, _) = frame.argb.as_chunks::<4>();
    let (target_pixels, _) = pixmap.data_mut().as_chunks_mut::<4>();
    for (source, target) in source_pixels.iter().zip(target_pixels.iter_mut()) {
        target[0] = source[2];
        target[1] = source[1];
        target[2] = source[0];
        target[3] = source[3];
    }
    pixmap.encode_png().map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)]
fn draw_item_text(
    context: &mut RenderContext,
    pixmap: &mut Pixmap,
    item: &skin::SkinItem,
    text: &str,
    color: (u8, u8, u8, u8),
    scale: f32,
    rect: (i32, i32, i32, i32),
    skin: &Skin,
) {
    let (x, y, width, height) = rect;
    let font_size = skin.font_size_for(item) * scale;
    let line_height = font_size * 1.4;
    let mut buffer = Buffer::new(
        &mut context.font_system,
        Metrics::new(font_size, line_height),
    );
    buffer.set_size(
        &mut context.font_system,
        Some(width as f32),
        Some(height as f32),
    );
    let attrs = Attrs::new().weight(Weight::MEDIUM);
    let attrs = if item.mono {
        attrs.family(Family::Monospace)
    } else {
        attrs.family(Family::Name(&skin.font.family))
    };
    buffer.set_text(&mut context.font_system, text, attrs, Shaping::Advanced);
    let line_width = buffer
        .layout_runs()
        .map(|run| run.line_w)
        .fold(0.0_f32, f32::max);
    let offset_x = match item.align {
        skin::Align::Left => 0.0,
        skin::Align::Right => (width as f32 - line_width).max(0.0),
        skin::Align::Center => ((width as f32 - line_width) / 2.0).max(0.0),
    };
    let offset_y = ((height as f32 - line_height) / 2.0).max(0.0);
    let x0 = x as f32 + offset_x;
    let y0 = y as f32 + offset_y;
    let (r, g, b, a) = color;
    buffer.draw(
        &mut context.font_system,
        &mut context.swash_cache,
        TextColor::rgba(r, g, b, a),
        |gx, gy, glyph_width, glyph_height, glyph_color| {
            blend_glyph(
                pixmap,
                x0 as i32 + gx,
                y0 as i32 + gy,
                glyph_width,
                glyph_height,
                glyph_color,
            );
        },
    );
}

fn parse_color_rgba(value: &str) -> (u8, u8, u8, u8) {
    let (r, g, b) = parse_color(value);
    let hex = value.trim_start_matches('#');
    let a = if hex.len() == 8 {
        u8::from_str_radix(&hex[6..8], 16).unwrap_or(255)
    } else {
        255
    };
    (r, g, b, a)
}

pub fn layout(items: &[DisplayItem], snapshot: &Snapshot, dpi: u32) -> Layout {
    let scale = (dpi.max(1) as f32) / BASE_DPI as f32;
    let width = BASE_WIDTH * scale;
    let row_height = ROW_HEIGHT * scale;
    let padding = PADDING * scale;
    let gap = GAP * scale;
    let col_width = (width - padding * 2.0 - gap) / 2.0;

    let visible_items: Vec<&DisplayItem> = items
        .iter()
        .filter(|item| item.visible && value::is_available(&item.id, snapshot))
        .collect();
    let num_rows = (visible_items.len() as f32 / 2.0).ceil() as usize;

    let mut layout_items = Vec::new();
    for (index, item) in visible_items.iter().enumerate() {
        let col = index % 2;
        let row = index / 2;
        let value = item_value_text(item, snapshot);
        let text = if item.label.is_empty() {
            value
        } else {
            format!("{} {}", item.label, value)
        };
        let x = padding + col as f32 * (col_width + gap);
        let y = padding + row as f32 * row_height;
        layout_items.push(LayoutItem {
            id: item.id.clone(),
            label: item.label.clone(),
            text,
            color: item.color.clone(),
            x,
            y,
            width: col_width,
            height: row_height,
        });
    }
    let height = (padding * 2.0 + num_rows as f32 * row_height).max(1.0);
    Layout {
        width: width.round() as u32,
        height: height.round() as u32,
        items: layout_items,
        scale,
    }
}

pub fn value_text(id: &str, snapshot: &Snapshot) -> String {
    match id {
        "net_up" => format_rate(snapshot.net.tx_bps),
        "net_down" => format_rate(snapshot.net.rx_bps),
        "cpu" => format!("{:.0}%", snapshot.cpu.total_pct),
        "mem" => format!("{:.0}%", snapshot.mem.used_pct),
        "cpu_temp" => snapshot
            .cpu_temp
            .map(|celsius| format!("{celsius:.0}°C"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// 显示项参与布局与缓存键的值文本；`format` 含 `{cpu_temp}` 时把温度内联进来。
/// 缓存键同样走这里，保证独立温度项被隐藏/删除后，温度变化仍会触发重绘。
fn item_value_text(item: &DisplayItem, snapshot: &Snapshot) -> String {
    let value = value_text(&item.id, snapshot);
    value::expand_cpu_temp_template(&item.format, &value, snapshot).unwrap_or(value)
}

struct RenderContext {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

fn context() -> &'static Mutex<RenderContext> {
    static CONTEXT: OnceLock<Mutex<RenderContext>> = OnceLock::new();
    CONTEXT.get_or_init(|| {
        Mutex::new(RenderContext {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        })
    })
}

pub fn render(snapshot: &Snapshot, config: &Config, dpi: u32) -> RenderedFrame {
    render_with_scheme(snapshot, config, dpi, ColorScheme::Dark)
}

/// 固定布局渲染；主题只影响背景与缓存键，落点几何不变。
pub fn render_with_scheme(
    snapshot: &Snapshot,
    config: &Config,
    dpi: u32,
    scheme: ColorScheme,
) -> RenderedFrame {
    render_with_scheme_and_alerts(snapshot, config, dpi, scheme, &AlertState::default())
}

pub fn render_with_scheme_and_alerts(
    snapshot: &Snapshot,
    config: &Config,
    dpi: u32,
    scheme: ColorScheme,
    alerts: &AlertState,
) -> RenderedFrame {
    let mut layout = layout(&config.overlay.items, snapshot, dpi);
    for item in &mut layout.items {
        if let Some(color) = alerts.color_for(&item.id) {
            item.color = color.to_owned();
        }
    }
    let cache_key = cache_key(snapshot, config, dpi, scheme, alerts);
    let mut pixmap = Pixmap::new(layout.width, layout.height).expect("non-zero overlay size");
    pixmap.fill(Color::TRANSPARENT);

    let opacity = config.overlay.opacity.clamp(0.0, 1.0);
    let background = scheme_background(scheme);
    let mut paint = Paint::default();
    paint.set_color_rgba8(background.0, background.1, background.2, 255);
    paint.anti_alias = true;
    let mut builder = PathBuilder::new();
    let radius = 10.0 * layout.scale;
    rounded_rect(
        &mut builder,
        layout.width as f32,
        layout.height as f32,
        radius,
    );
    if let Some(path) = builder.finish() {
        pixmap.fill_path(
            &path,
            &paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    draw_border(&mut pixmap, layout.width, layout.height, radius, scheme);

    let mut guard = context().lock().expect("render context poisoned");
    let context = &mut *guard;
    for item in &layout.items {
        if item.text.is_empty() {
            continue;
        }
        let (r, g, b) = readable_color(parse_color(&item.color), background, opacity, MIN_CONTRAST);
        let font_size = FONT_SIZE * layout.scale;
        let line_height = font_size * 1.4;
        let mut buffer = Buffer::new(
            &mut context.font_system,
            Metrics::new(font_size, line_height),
        );
        buffer.set_size(
            &mut context.font_system,
            Some(item.width),
            Some(item.height),
        );
        buffer.set_text(
            &mut context.font_system,
            &item.text,
            Attrs::new().weight(Weight::MEDIUM),
            Shaping::Advanced,
        );
        let x0 = item.x;
        let y0 = item.y + ((item.height - line_height) / 2.0).max(0.0);
        buffer.draw(
            &mut context.font_system,
            &mut context.swash_cache,
            TextColor::rgb(r, g, b),
            |x, y, width, height, color| {
                blend_glyph(
                    &mut pixmap,
                    x0 as i32 + x,
                    y0 as i32 + y,
                    width,
                    height,
                    color,
                );
            },
        );
    }

    // 整体透明度在合成完成后统一作用，避免文字与背景叠加放大 alpha。
    let factor = (opacity * 255.0).round() as u32;
    if factor < 255 {
        for byte in pixmap.data_mut() {
            *byte = ((*byte as u32 * factor + 127) / 255) as u8;
        }
    }

    // RGBA → BGRA（保留预乘 alpha）。
    let mut bgra = Vec::with_capacity(layout.width as usize * layout.height as usize * 4);
    for pixel in pixmap.data().as_chunks::<4>().0 {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    let regions = layout
        .items
        .iter()
        .map(|item| HitRegion {
            x: item.x as i32,
            y: item.y as i32,
            width: item.width as i32,
            height: item.height as i32,
        })
        .collect();
    RenderedFrame {
        width: layout.width,
        height: layout.height,
        argb: bgra,
        cache_key,
        regions,
    }
}

fn cache_key(
    snapshot: &Snapshot,
    config: &Config,
    dpi: u32,
    scheme: ColorScheme,
    alerts: &AlertState,
) -> String {
    let mut key = String::new();
    key.push_str(&format!(
        "{dpi}:{:.3}:{}:",
        config.overlay.opacity,
        match scheme {
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
        }
    ));
    for item in &config.overlay.items {
        key.push_str(&item.id);
        key.push('=');
        key.push_str(&item_value_text(item, snapshot));
        key.push(',');
    }
    key.push_str(&alerts.cache_key());
    key
}

fn rounded_rect(builder: &mut PathBuilder, width: f32, height: f32, radius: f32) {
    rounded_rect_at(builder, 0.0, 0.0, width, height, radius);
}

fn rounded_rect_at(
    builder: &mut PathBuilder,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    radius: f32,
) {
    let r = radius.min(width / 2.0).min(height / 2.0);
    builder.move_to(x + r, y);
    builder.line_to(x + width - r, y);
    builder.quad_to(x + width, y, x + width, y + r);
    builder.line_to(x + width, y + height - r);
    builder.quad_to(x + width, y + height, x + width - r, y + height);
    builder.line_to(x + r, y + height);
    builder.quad_to(x, y + height, x, y + height - r);
    builder.line_to(x, y + r);
    builder.quad_to(x, y, x + r, y);
    builder.close();
}

/// 内描边让悬浮窗在纯色壁纸上也有边界，不依赖外部阴影。
fn draw_border(pixmap: &mut Pixmap, width: u32, height: u32, radius: f32, scheme: ColorScheme) {
    let (r, g, b, a) = match scheme {
        ColorScheme::Dark => (255, 255, 255, 26),
        ColorScheme::Light => (0, 0, 0, 22),
    };
    let inset = 0.5;
    let mut builder = PathBuilder::new();
    rounded_rect_at(
        &mut builder,
        inset,
        inset,
        width as f32 - inset * 2.0,
        height as f32 - inset * 2.0,
        (radius - inset).max(0.0),
    );
    let Some(path) = builder.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    let stroke = Stroke {
        width: 1.0,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

fn scheme_background(scheme: ColorScheme) -> (u8, u8, u8) {
    match scheme {
        ColorScheme::Dark => (16, 18, 22),
        ColorScheme::Light => (242, 244, 248),
    }
}

/// 背景图按 alpha 加权的平均色；全透明时返回 `None`。
fn average_rgb(pixmap: &Pixmap) -> Option<(u8, u8, u8)> {
    // 像素是预乘 alpha：sum(channel * 255) / sum(alpha) 等价于非预乘均值。
    let mut sums = [0u64; 3];
    let mut weight = 0u64;
    for pixel in pixmap.data().as_chunks::<4>().0 {
        let alpha = u64::from(pixel[3]);
        if alpha == 0 {
            continue;
        }
        for (sum, channel) in sums.iter_mut().zip(pixel) {
            *sum += u64::from(*channel) * 255;
        }
        weight += alpha;
    }
    if weight == 0 {
        return None;
    }
    let channel = |sum: u64| ((sum + weight / 2) / weight).min(255) as u8;
    Some((channel(sums[0]), channel(sums[1]), channel(sums[2])))
}

/// 保证文字在给定背景与透明度下达到最低对比度：保持色相与饱和度，只调整亮度。
fn readable_color(
    foreground: (u8, u8, u8),
    background: (u8, u8, u8),
    alpha: f32,
    minimum: f32,
) -> (u8, u8, u8) {
    let alpha = alpha.clamp(0.0, 1.0);
    if contrast_ratio(composite(foreground, background, alpha), background) >= minimum {
        return foreground;
    }
    let (hue, saturation, lightness) = rgb_to_hsl(foreground);
    let light_background = relative_luminance(background) > 0.5;
    for step in 1..=100 {
        let t = step as f32 / 100.0;
        let candidate_lightness = if light_background {
            lightness * (1.0 - t)
        } else {
            lightness + (1.0 - lightness) * t
        };
        let candidate = hsl_to_rgb(hue, saturation, candidate_lightness);
        if contrast_ratio(composite(candidate, background, alpha), background) >= minimum {
            return candidate;
        }
    }
    if light_background {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

fn composite(foreground: (u8, u8, u8), background: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let mix = |fg: u8, bg: u8| {
        (f32::from(fg) * alpha + f32::from(bg) * (1.0 - alpha))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (
        mix(foreground.0, background.0),
        mix(foreground.1, background.1),
        mix(foreground.2, background.2),
    )
}

/// WCAG 相对亮度。
fn relative_luminance((r, g, b): (u8, u8, u8)) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    let (high, low) = if a > b { (a, b) } else { (b, a) };
    (high + 0.05) / (low + 0.05)
}

fn rgb_to_hsl((r, g, b): (u8, u8, u8)) -> (f32, f32, f32) {
    let r = f32::from(r) / 255.0;
    let g = f32::from(g) / 255.0;
    let b = f32::from(b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let lightness = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, lightness);
    }
    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let hue = if max == r {
        ((g - b) / delta + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };
    (hue * 360.0, saturation, lightness)
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let to_byte = |value: f32| (value * 255.0).round().clamp(0.0, 255.0) as u8;
    if saturation <= f32::EPSILON {
        let value = to_byte(lightness);
        return (value, value, value);
    }
    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    let hue = hue.rem_euclid(360.0) / 360.0;
    let channel = |offset: f32| {
        let t = (hue + offset).rem_euclid(1.0);
        let value = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        to_byte(value)
    };
    (channel(1.0 / 3.0), channel(0.0), channel(-1.0 / 3.0))
}

fn parse_color(value: &str) -> (u8, u8, u8) {
    let hex = value.trim_start_matches('#');
    if hex.len() >= 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255);
        (r, g, b)
    } else {
        (255, 255, 255)
    }
}

fn blend_glyph(pixmap: &mut Pixmap, x: i32, y: i32, width: u32, height: u32, color: TextColor) {
    if color.a() == 0 {
        return;
    }
    let stride = pixmap.width() as usize;
    let pixmap_height = pixmap.height() as i32;
    let data = pixmap.data_mut();
    let alpha = color.a() as u32;
    let inv = 255 - alpha;
    for dy in 0..height as usize {
        let py = y + dy as i32;
        if py < 0 || py >= pixmap_height {
            continue;
        }
        for dx in 0..width as usize {
            let px = x + dx as i32;
            if px < 0 || px >= stride as i32 {
                continue;
            }
            let offset = (py as usize * stride + px as usize) * 4;
            let src = [
                (color.r() as u32 * alpha + 127) / 255,
                (color.g() as u32 * alpha + 127) / 255,
                (color.b() as u32 * alpha + 127) / 255,
            ];
            for channel in 0..3 {
                let dst = data[offset + channel] as u32;
                data[offset + channel] = ((src[channel] * 255 + dst * inv + 127) / 255) as u8;
            }
            let dst_a = data[offset + 3] as u32;
            data[offset + 3] = ((alpha * 255 + dst_a * inv + 127) / 255) as u8;
        }
    }
}

/// 22×22 SNI 图标：ARGB 网络字节序（A,R,G,B），两行条形表示上下行。
#[derive(Debug, Clone, PartialEq)]
pub struct TrayIcon {
    pub width: i32,
    pub height: i32,
    pub argb: Vec<u8>,
}

pub fn tray_icon(snapshot: &Snapshot) -> TrayIcon {
    const SIZE: usize = 22;
    let mut pixels = vec![0u8; SIZE * SIZE * 4];
    draw_bar(
        &mut pixels,
        SIZE,
        5,
        snapshot.net.rx_bps,
        [0x8C, 0xE9, 0x9A],
    );
    draw_bar(
        &mut pixels,
        SIZE,
        13,
        snapshot.net.tx_bps,
        [0x7F, 0xD1, 0xFF],
    );
    TrayIcon {
        width: SIZE as i32,
        height: SIZE as i32,
        argb: pixels,
    }
}

fn draw_bar(pixels: &mut [u8], size: usize, y: usize, rate: u64, color: [u8; 3]) {
    let width = ((rate.max(1) as f64).log10() * 3.0)
        .round()
        .clamp(2.0, 18.0) as usize;
    let x0 = (size - width) / 2;
    for dy in 0..4 {
        for x in x0..(x0 + width).min(size) {
            let offset = ((y + dy) * size + x) * 4;
            pixels[offset] = 255;
            pixels[offset + 1] = color[0];
            pixels[offset + 2] = color[1];
            pixels[offset + 3] = color[2];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trafficview_core::{CpuSample, MemSample, NetSample};

    fn snapshot() -> Snapshot {
        Snapshot::new(
            1,
            NetSample {
                iface: "eth0".into(),
                rx_bps: 1024,
                tx_bps: 2048,
                rx_total: 1,
                tx_total: 2,
            },
            CpuSample {
                total_pct: 12.0,
                per_core: vec![12.0],
                freq_mhz: None,
                load: None,
            },
            MemSample {
                used_pct: 34.0,
                total: 1,
                available: 1,
                swap_pct: 0.0,
            },
        )
    }

    #[test]
    fn light_background_text_is_darkened_to_meet_contrast() {
        let background = (242, 244, 248);
        let adapted = readable_color((255, 212, 59), background, 0.8, MIN_CONTRAST);
        let visible = composite(adapted, background, 0.8);
        assert!(
            contrast_ratio(visible, background) >= MIN_CONTRAST,
            "adapted color {adapted:?} is still low contrast"
        );
        assert!(relative_luminance(adapted) < relative_luminance((255, 212, 59)));
    }

    #[test]
    fn dark_background_keeps_readable_palette_unchanged() {
        let background = (16, 18, 22);
        assert_eq!(
            readable_color((127, 209, 255), background, 0.8, MIN_CONTRAST),
            (127, 209, 255)
        );
    }

    #[test]
    fn tray_icon_is_argb_and_scales_with_traffic() {
        let snapshot = snapshot();
        let icon = tray_icon(&snapshot);
        assert_eq!((icon.width, icon.height), (22, 22));
        assert_eq!(icon.argb.len(), 22 * 22 * 4);
        let opaque = icon
            .argb
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[0] > 0)
            .count();
        assert!(opaque > 0);
        let mut faster = snapshot.clone();
        faster.net.rx_bps = 10 * 1024 * 1024;
        let fast_opaque = tray_icon(&faster)
            .argb
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[0] > 0)
            .count();
        assert!(fast_opaque >= opaque);
    }

    #[test]
    fn fixed_layout_is_deterministic_and_scaled() {
        let items = Config::default().overlay.items;
        let a = layout(&items, &snapshot(), 96);
        let b = layout(&items, &snapshot(), 192);
        assert_eq!(b.width, a.width * 2);
        assert_eq!(b.height, a.height * 2);
        assert_eq!(
            a.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["net_up", "net_down", "cpu", "mem"]
        );
    }

    #[test]
    fn empty_snapshot_and_empty_items_do_not_panic() {
        let empty = Snapshot::new(
            0,
            NetSample {
                iface: String::new(),
                rx_bps: 0,
                tx_bps: 0,
                rx_total: 0,
                tx_total: 0,
            },
            CpuSample {
                total_pct: 0.0,
                per_core: vec![],
                freq_mhz: None,
                load: None,
            },
            MemSample {
                used_pct: 0.0,
                total: 0,
                available: 0,
                swap_pct: 0.0,
            },
        );
        let mut config = Config::default();
        config.overlay.items.clear();
        let frame = render(&empty, &config, 96);
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(frame.argb.len(), (frame.width * frame.height * 4) as usize);
    }

    #[test]
    fn cpu_temp_item_is_hidden_until_a_reading_exists() {
        let items = Config::default().overlay.items;
        let mut snapshot = snapshot();

        let without = layout(&items, &snapshot, 96);
        assert!(!without.items.iter().any(|item| item.id == "cpu_temp"));

        snapshot.cpu_temp = Some(66.4);
        let with = layout(&items, &snapshot, 96);
        let item = with
            .items
            .iter()
            .find(|item| item.id == "cpu_temp")
            .expect("cpu_temp item");
        assert_eq!(item.text, "TEMP 66°C");
        assert!(with.height > without.height);
    }

    #[test]
    fn cpu_temp_display_value_uses_integer_celsius() {
        let mut snapshot = snapshot();
        snapshot.cpu_temp = Some(66.4);
        assert_eq!(
            display_value(
                "cpu_temp",
                &snapshot,
                trafficview_core::config::RateUnit::Bytes,
                "{value}"
            ),
            "66°C"
        );
    }

    #[test]
    fn cpu_item_can_inline_cpu_temp_in_builtin_layout() {
        let mut items = Config::default().overlay.items;
        items
            .iter_mut()
            .find(|item| item.id == "cpu")
            .unwrap()
            .format = "{value} {cpu_temp}".into();
        items
            .iter_mut()
            .find(|item| item.id == "cpu_temp")
            .unwrap()
            .visible = false;

        let mut snapshot = snapshot();
        snapshot.cpu.total_pct = 14.0;

        let without_temp = layout(&items, &snapshot, 96);
        assert_eq!(without_temp.items.len(), 4);
        let cpu = without_temp
            .items
            .iter()
            .find(|item| item.id == "cpu")
            .unwrap();
        assert_eq!(cpu.text, "CPU 14%");

        snapshot.cpu_temp = Some(70.4);
        let with_temp = layout(&items, &snapshot, 96);
        assert_eq!(with_temp.items.len(), 4);
        let cpu = with_temp
            .items
            .iter()
            .find(|item| item.id == "cpu")
            .unwrap();
        assert_eq!(cpu.text, "CPU 14% 70°C");
        assert_eq!(with_temp.height, without_temp.height);
    }

    #[test]
    fn display_value_supports_cpu_temp_placeholder() {
        let mut snapshot = snapshot();
        snapshot.cpu.total_pct = 14.0;
        snapshot.cpu_temp = Some(70.4);
        assert_eq!(
            display_value(
                "cpu",
                &snapshot,
                trafficview_core::config::RateUnit::Bytes,
                "{value} {cpu_temp}"
            ),
            "14.0% 70°C"
        );

        snapshot.cpu_temp = None;
        assert_eq!(
            display_value(
                "cpu",
                &snapshot,
                trafficview_core::config::RateUnit::Bytes,
                "{value} {cpu_temp}"
            ),
            "14.0%"
        );

        // 有温度读数时保留模板里的多个空格；无读数时压缩空白。
        snapshot.cpu_temp = Some(70.4);
        assert_eq!(
            display_value(
                "cpu",
                &snapshot,
                trafficview_core::config::RateUnit::Bytes,
                "{value}  {cpu_temp}"
            ),
            "14.0%  70°C"
        );
        snapshot.cpu_temp = None;
        assert_eq!(
            display_value(
                "cpu",
                &snapshot,
                trafficview_core::config::RateUnit::Bytes,
                "{value}  {cpu_temp}"
            ),
            "14.0%"
        );
    }

    #[test]
    fn cache_key_is_stable_for_unchanged_text() {
        let snapshot = snapshot();
        let config = Config::default();
        assert_eq!(
            render(&snapshot, &config, 96).cache_key,
            render(&snapshot, &config, 96).cache_key
        );
        let mut changed = snapshot.clone();
        changed.net.rx_bps += 1024;
        assert_ne!(
            render(&snapshot, &config, 96).cache_key,
            render(&changed, &config, 96).cache_key
        );
    }

    #[test]
    fn opacity_is_written_to_alpha_and_hit_regions_stay_inside() {
        let config = Config {
            overlay: trafficview_core::config::OverlayConfig {
                opacity: 0.5,
                ..Default::default()
            },
            ..Config::default()
        };
        let frame = render(&snapshot(), &config, 96);
        let max_alpha = frame
            .argb
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[3])
            .max()
            .unwrap_or(0);
        assert!(max_alpha <= 128, "alpha {max_alpha} exceeds opacity");
        for region in &frame.regions {
            assert!(region.x >= 0 && region.y >= 0);
            assert!(region.x + region.width <= frame.width as i32);
            assert!(region.y + region.height <= frame.height as i32);
        }
    }
}
