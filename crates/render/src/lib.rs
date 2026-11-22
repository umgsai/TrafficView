//! 平台无关的固定布局 ARGB 渲染。96 DPI 为基准，输出 BGRA 预乘字节。

pub mod nine_patch;
pub mod skin;
pub mod value;

use std::sync::{Mutex, OnceLock};

use cosmic_text::{
    Attrs, Buffer, Color as TextColor, Family, FontSystem, Metrics, Shaping, SwashCache,
};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Transform};
use trafficview_core::{
    config::{Config, DisplayItem},
    format::format_rate,
    Snapshot,
};

pub use skin::{Skin, SkinError};
pub use value::{display_value, is_known_id, numeric_value};

pub const BASE_DPI: u32 = 96;
const ROW_HEIGHT: f32 = 24.0;
const PADDING: f32 = 8.0;
const BASE_WIDTH: f32 = 220.0;
const FONT_SIZE: f32 = 14.0;

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
    last_key: Option<String>,
    last_frame: Option<RenderedFrame>,
    rasterize_count: u64,
}

impl Renderer {
    pub fn new(skin: Skin) -> Result<Self, SkinError> {
        let background = match &skin.background_path {
            Some(path) => Some(Pixmap::load_png(path).map_err(|error| {
                SkinError::Invalid(format!(
                    "cannot decode background '{}': {error}",
                    path.display()
                ))
            })?),
            None => None,
        };
        if let Some(insets) = skin.canvas.nine_patch {
            let Some(background) = &background else {
                return Err(SkinError::Invalid(
                    "nine_patch requires a background PNG".into(),
                ));
            };
            nine_patch::validate_insets(background.width(), background.height(), insets)
                .map_err(SkinError::Invalid)?;
        }
        Ok(Self {
            skin,
            background,
            last_key: None,
            last_frame: None,
            rasterize_count: 0,
        })
    }

    pub fn skin(&self) -> &Skin {
        &self.skin
    }

    /// 实际栅格化次数；缓存命中不增加。
    pub fn rasterize_count(&self) -> u64 {
        self.rasterize_count
    }

    pub fn render(&mut self, snapshot: &Snapshot, scale: f32) -> Result<RenderedFrame, SkinError> {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let key = self.cache_key(snapshot, scale);
        if self.last_key.as_deref() == Some(key.as_str()) {
            if let Some(frame) = &self.last_frame {
                return Ok(frame.clone());
            }
        }
        let frame = self.rasterize(snapshot, scale)?;
        self.last_key = Some(key);
        self.last_frame = Some(frame.clone());
        self.rasterize_count += 1;
        Ok(frame)
    }

    fn cache_key(&self, snapshot: &Snapshot, scale: f32) -> String {
        let mut key = format!(
            "{:016x}:{scale:.4}:{:.3}:",
            self.skin.content_hash, self.skin.canvas.opacity
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
        key
    }

    fn rasterize(&self, snapshot: &Snapshot, scale: f32) -> Result<RenderedFrame, SkinError> {
        let width = ((self.skin.canvas.width as f32) * scale).round().max(1.0) as u32;
        let height = ((self.skin.canvas.height as f32) * scale).round().max(1.0) as u32;
        let mut pixmap = Pixmap::new(width, height)
            .ok_or_else(|| SkinError::Invalid("canvas size must not be zero".into()))?;

        if let Some(background) = &self.background {
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
        let mut guard = context().lock().expect("render context poisoned");
        let context = &mut *guard;
        for item in &self.skin.items {
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
            let color = match value::numeric_value(&item.id, snapshot) {
                Some(value) => item.color_for(value),
                None => item.color.clone(),
            };
            draw_item_text(
                context,
                &mut pixmap,
                item,
                &text,
                &color,
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
            cache_key: self.cache_key(snapshot, scale),
            regions,
        })
    }
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
    color: &str,
    scale: f32,
    rect: (i32, i32, i32, i32),
    skin: &Skin,
) {
    let (x, y, width, height) = rect;
    let font_size = skin.font_size_for(item) * scale;
    let mut buffer = Buffer::new(
        &mut context.font_system,
        Metrics::new(font_size, font_size * 1.4),
    );
    buffer.set_size(
        &mut context.font_system,
        Some(width as f32),
        Some(height as f32),
    );
    let attrs = if item.mono {
        Attrs::new().family(Family::Monospace)
    } else {
        Attrs::new().family(Family::Name(&skin.font.family))
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
    let offset_y = ((height as f32 - font_size * 1.4) / 2.0).max(0.0);
    let x0 = x as f32 + offset_x;
    let y0 = y as f32 + offset_y;
    let (r, g, b, a) = parse_color_rgba(color);
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
    let mut rows = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !item.visible {
            continue;
        }
        let value = value_text(&item.id, snapshot);
        let text = if item.label.is_empty() {
            value
        } else {
            format!("{} {}", item.label, value)
        };
        rows.push(LayoutItem {
            id: item.id.clone(),
            label: item.label.clone(),
            text,
            color: item.color.clone(),
            x: padding,
            y: padding + index as f32 * row_height,
            width: width - padding * 2.0,
            height: row_height,
        });
    }
    let height = (padding * 2.0 + rows.len() as f32 * row_height).max(1.0);
    Layout {
        width: width.round() as u32,
        height: height.round() as u32,
        items: rows,
        scale,
    }
}

pub fn value_text(id: &str, snapshot: &Snapshot) -> String {
    match id {
        "net_up" => format_rate(snapshot.net.tx_bps),
        "net_down" => format_rate(snapshot.net.rx_bps),
        "cpu" => format!("{:.0}%", snapshot.cpu.total_pct),
        "mem" => format!("{:.0}%", snapshot.mem.used_pct),
        _ => String::new(),
    }
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
    let layout = layout(&config.overlay.items, snapshot, dpi);
    let cache_key = cache_key(snapshot, config, dpi);
    let mut pixmap = Pixmap::new(layout.width, layout.height).expect("non-zero overlay size");
    pixmap.fill(Color::TRANSPARENT);

    let opacity = config.overlay.opacity.clamp(0.0, 1.0);
    let mut paint = Paint::default();
    paint.set_color_rgba8(16, 18, 22, 255);
    paint.anti_alias = true;
    let mut builder = PathBuilder::new();
    let radius = 8.0 * layout.scale;
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

    let mut guard = context().lock().expect("render context poisoned");
    let context = &mut *guard;
    for item in &layout.items {
        if item.text.is_empty() {
            continue;
        }
        let (r, g, b) = parse_color(&item.color);
        let font_size = FONT_SIZE * layout.scale;
        let mut buffer = Buffer::new(
            &mut context.font_system,
            Metrics::new(font_size, font_size * 1.4),
        );
        buffer.set_size(
            &mut context.font_system,
            Some(item.width),
            Some(item.height),
        );
        buffer.set_text(
            &mut context.font_system,
            &item.text,
            Attrs::new(),
            Shaping::Advanced,
        );
        let x0 = item.x;
        let y0 = item.y;
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

fn cache_key(snapshot: &Snapshot, config: &Config, dpi: u32) -> String {
    let mut key = String::new();
    key.push_str(&format!("{dpi}:{:.3}:", config.overlay.opacity));
    for item in &config.overlay.items {
        key.push_str(&item.id);
        key.push('=');
        key.push_str(&value_text(&item.id, snapshot));
        key.push(',');
    }
    key
}

fn rounded_rect(builder: &mut PathBuilder, width: f32, height: f32, radius: f32) {
    let rect = Rect::from_xywh(0.0, 0.0, width, height)
        .unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap());
    builder.push_rect(rect);
    let _ = (builder, radius);
}

fn parse_color(value: &str) -> (u8, u8, u8) {
    let hex = value.trim_start_matches('#');
    if hex.len() == 6 {
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
