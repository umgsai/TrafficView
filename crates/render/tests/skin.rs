use std::path::{Path, PathBuf};

use trafficview_core::{
    config::RateUnit, format::format_rate, CpuSample, MemSample, NetSample, Snapshot,
};
use trafficview_render::{
    skin::{Skin, SkinItem},
    value::display_value,
    Renderer,
};

fn test_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn snapshot() -> Snapshot {
    Snapshot::new(
        1,
        NetSample {
            iface: "eth0".into(),
            rx_bps: 1024,
            tx_bps: 2048,
            rx_total: 10,
            tx_total: 20,
        },
        CpuSample {
            total_pct: 12.0,
            per_core: vec![12.0],
            freq_mhz: None,
            load: None,
        },
        MemSample {
            used_pct: 34.0,
            total: 100,
            available: 66,
            swap_pct: 0.0,
        },
    )
}

#[test]
fn parses_research_skin_model() {
    let skin = Skin::load(test_path("minimal")).unwrap();
    assert_eq!(skin.meta.name, "Minimal");
    assert_eq!(skin.items[0].id, "net_up");
    assert_eq!(skin.items[0].rect, [10, 6, 92, 20]);
    assert!(skin.items[0].mono);
    assert_eq!(skin.items[0].color, "#FFFFFF");
    assert!(skin.warnings.is_empty());
}

#[test]
fn rejects_invalid_rectangles_and_thresholds() {
    for source in [
        include_str!("fixtures/invalid/zero-width.toml"),
        include_str!("fixtures/invalid/duplicate-id.toml"),
        include_str!("fixtures/invalid/unsorted-thresholds.toml"),
    ] {
        assert!(
            Skin::parse(source, Path::new("fixture")).is_err(),
            "{source}"
        );
    }
}

#[test]
fn background_must_stay_inside_skin_directory() {
    let error = Skin::parse(
        "[meta]\nname='x'\n[canvas]\nbackground='../../secret.png'",
        Path::new("fixture"),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("outside skin directory"),
        "{error}"
    );
}

#[test]
fn unknown_item_id_warns_without_failing() {
    let skin = Skin::parse(
        "[meta]\nname='x'\n[canvas]\nwidth=100\nheight=40\n[[item]]\nid='bogus'\nrect=[0,0,10,10]",
        Path::new("fixture"),
    )
    .unwrap();
    assert_eq!(skin.items.len(), 1);
    assert_eq!(skin.warnings.len(), 1);
    assert!(skin.warnings[0].contains("bogus"));
}

#[test]
fn threshold_boundaries_are_inclusive() {
    let item = SkinItem::threshold_test(&[(80.0, "#FFC107"), (90.0, "#F44336")]);
    assert_eq!(item.color_for(79.9), item.color);
    assert_eq!(item.color_for(80.0), "#FFC107");
    assert_eq!(item.color_for(90.0), "#F44336");
}

#[test]
fn formats_only_replace_the_value_variable() {
    let snapshot = snapshot();
    let text = display_value("net_up", &snapshot, RateUnit::Bytes, "↑ {value} / {other}");
    assert_eq!(text, format!("↑ {} / {{other}}", format_rate(2048)));
    let bits = display_value("net_down", &snapshot, RateUnit::Bits, "{value}");
    assert_eq!(bits, "8.0 Kb/s");
}

#[test]
fn unchanged_frame_is_not_rasterized_again() {
    let skin = Skin::load(test_path("minimal")).unwrap();
    let mut renderer = Renderer::new(skin).unwrap();
    let first = renderer.render(&snapshot(), 1.0).unwrap();
    let second = renderer.render(&snapshot(), 1.0).unwrap();
    assert_eq!(first.cache_key, second.cache_key);
    assert_eq!(renderer.rasterize_count(), 1);
    assert_eq!((first.width, first.height), (220, 80));
}

#[test]
fn opaque_background_produces_visible_pixels() {
    let skin = Skin::load(test_path("minimal")).unwrap();
    let mut renderer = Renderer::new(skin).unwrap();
    let frame = renderer.render(&snapshot(), 1.0).unwrap();
    let max_alpha = frame
        .argb
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| pixel[3])
        .max()
        .unwrap_or(0);
    assert!(max_alpha > 0, "canvas should not be fully transparent");
    assert_eq!(frame.argb.len(), (frame.width * frame.height * 4) as usize);
    let png = trafficview_render::encode_png(&frame).unwrap();
    assert_eq!(&png[1..4], b"PNG");
}

#[test]
fn transparent_background_keeps_zero_alpha() {
    let skin = Skin::parse(
        "[meta]\nname='transparent'\n[canvas]\nwidth=64\nheight=24\n[[item]]\nid='cpu'\nrect=[0,0,64,24]",
        Path::new("fixture"),
    )
    .unwrap();
    let mut renderer = Renderer::new(skin).unwrap();
    let frame = renderer.render(&snapshot(), 1.0).unwrap();
    let opaque = frame
        .argb
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] > 0)
        .count();
    let total = (frame.width * frame.height) as usize;
    assert!(opaque > 0 && opaque < total, "text pixels should be sparse");
}

#[test]
fn color_scheme_changes_the_cache_key_but_not_item_geometry() {
    let skin = Skin::parse(
        "[meta]\nname='theme'\n[canvas]\nwidth=120\nheight=40\nbackground_color='#101010'\n[[item]]\nid='cpu'\nrect=[4,4,112,32]",
        Path::new("fixture"),
    )
    .unwrap();
    let mut renderer = Renderer::new(skin).unwrap();
    let dark = renderer.render(&snapshot(), 1.0).unwrap();
    renderer.set_color_scheme(trafficview_core::ColorScheme::Light);
    let light = renderer.render(&snapshot(), 1.0).unwrap();
    assert_ne!(dark.cache_key, light.cache_key);
    assert_eq!(dark.regions, light.regions);
    assert_eq!((dark.width, dark.height), (light.width, light.height));
}

#[test]
fn fixed_layout_render_includes_scheme_in_the_cache_key() {
    let config = trafficview_core::config::Config::default();
    let dark = trafficview_render::render_with_scheme(
        &snapshot(),
        &config,
        96,
        trafficview_core::ColorScheme::Dark,
    );
    let light = trafficview_render::render_with_scheme(
        &snapshot(),
        &config,
        96,
        trafficview_core::ColorScheme::Light,
    );
    assert_ne!(dark.cache_key, light.cache_key);
}

#[test]
fn alert_events_map_to_items_and_recovery_clears_only_that_alert() {
    let mut alerts = trafficview_render::AlertState::default();
    assert!(alerts.apply_event("info", "cpu.high", "CPU 使用率 95.0 超过阈值 90.0"));
    assert!(alerts.apply_event("warning", "mem.high", "内存使用率 95.0 超过阈值 90.0"));
    assert!(alerts.apply_event("warning", "net.rx.high", "下载速率超过阈值"));
    assert!(alerts.apply_event("critical", "net.tx.high", "上传速率超过阈值"));
    assert_eq!(alerts.color_for("cpu"), Some("#7FD1FF"));
    assert_eq!(alerts.color_for("mem"), Some("#FFC107"));
    assert_eq!(alerts.color_for("net_down"), Some("#FFC107"));
    assert_eq!(alerts.color_for("net_up"), Some("#F44336"));
    assert!(!alerts.apply_event("critical", "month.quota", "月流量超过配额"));

    assert!(alerts.apply_event("info", "cpu.high", "CPU 使用率 已恢复（当前 10.0）"));
    assert_eq!(alerts.color_for("cpu"), None);
    assert_eq!(alerts.color_for("mem"), Some("#FFC107"));
    assert!(!alerts.apply_event("info", "cpu.high", "CPU 使用率 已恢复（当前 10.0）"));
}

#[test]
fn skin_alert_changes_pixels_and_cache_then_recovery_restores_both() {
    let skin = Skin::parse(
        "[meta]\nname='alerts'\n[canvas]\nwidth=120\nheight=40\nbackground_color='#101010'\n[[item]]\nid='cpu'\nrect=[4,4,112,32]",
        Path::new("fixture"),
    )
    .unwrap();
    let mut renderer = Renderer::new(skin).unwrap();
    let normal = renderer.render(&snapshot(), 1.0).unwrap();
    let mut alerts = trafficview_render::AlertState::default();
    alerts.apply_event("critical", "cpu.high", "CPU 使用率 99.0 超过阈值 90.0");
    let alerted = renderer
        .render_with_alerts(&snapshot(), 1.0, &alerts)
        .unwrap();
    assert_ne!(normal.cache_key, alerted.cache_key);
    assert_ne!(normal.argb, alerted.argb);
    assert_eq!(normal.regions, alerted.regions);

    alerts.apply_event("info", "cpu.high", "CPU 使用率 已恢复（当前 10.0）");
    let recovered = renderer
        .render_with_alerts(&snapshot(), 1.0, &alerts)
        .unwrap();
    assert_eq!(normal.cache_key, recovered.cache_key);
    assert_eq!(normal.argb, recovered.argb);
}

#[test]
fn fixed_layout_alert_changes_pixels_and_cache_then_recovery_restores_both() {
    let config = trafficview_core::config::Config::default();
    let normal = trafficview_render::render_with_scheme(
        &snapshot(),
        &config,
        96,
        trafficview_core::ColorScheme::Dark,
    );
    let mut alerts = trafficview_render::AlertState::default();
    alerts.apply_event("warning", "net.rx.high", "下载速率超过阈值");
    let alerted = trafficview_render::render_with_scheme_and_alerts(
        &snapshot(),
        &config,
        96,
        trafficview_core::ColorScheme::Dark,
        &alerts,
    );
    assert_ne!(normal.cache_key, alerted.cache_key);
    assert_ne!(normal.argb, alerted.argb);
    assert_eq!(normal.regions, alerted.regions);

    alerts.apply_event("info", "net.rx.high", "下载速率 已恢复（当前 0.0）");
    let recovered = trafficview_render::render_with_scheme_and_alerts(
        &snapshot(),
        &config,
        96,
        trafficview_core::ColorScheme::Dark,
        &alerts,
    );
    assert_eq!(normal.cache_key, recovered.cache_key);
    assert_eq!(normal.argb, recovered.argb);
}
