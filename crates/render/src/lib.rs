//! 平台无关的固定布局 ARGB 渲染。96 DPI 为基准，输出 BGRA 预乘字节。

use std::sync::{Mutex, OnceLock};

use cosmic_text::{Attrs, Buffer, Color as TextColor, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Rect, Transform};
use trafficview_core::{
    config::{Config, DisplayItem},
    format::format_rate,
    Snapshot,
};

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
    for pixel in pixmap.data().chunks_exact(4) {
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
        let mut config = Config::default();
        config.overlay.opacity = 0.5;
        let frame = render(&snapshot(), &config, 96);
        let max_alpha = frame
            .argb
            .chunks_exact(4)
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
