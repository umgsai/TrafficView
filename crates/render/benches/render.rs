//! 单帧渲染基准：固定快照 + 默认深色皮肤 + 默认四项显示，不读写任何真实资源。

use std::collections::BTreeMap;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use trafficview_core::config::Config;
use trafficview_core::{CpuSample, MemSample, NetSample, Snapshot};
use trafficview_render::render;

fn fixture() -> Snapshot {
    Snapshot {
        ts_ms: 1_700_000_000_000,
        net: NetSample {
            iface: "enp0s3".into(),
            rx_bps: 12_345_678,
            tx_bps: 2_345_678,
            rx_total: 987_654_321_000,
            tx_total: 123_456_789_000,
        },
        cpu: CpuSample {
            total_pct: 42.5,
            per_core: vec![
                12.0, 24.0, 36.0, 48.0, 60.0, 72.0, 84.0, 96.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0,
                70.0, 80.0,
            ],
            freq_mhz: Some(2400),
            load: Some([0.82, 0.61, 0.47]),
        },
        mem: MemSample {
            used_pct: 61.0,
            total: 16 * 1024 * 1024 * 1024,
            available: 6 * 1024 * 1024 * 1024,
            swap_pct: 2.0,
        },
        disk: None,
        gpu: None,
        gpus: Vec::new(),
        temps: Vec::new(),
        filesystems: None,
        battery: None,
        uptime_s: Some(3600),
        processes: None,
        plugin: BTreeMap::new(),
    }
}

fn frame(c: &mut Criterion) {
    let config = Config::default();
    let snapshot = fixture();
    c.bench_function("render/default_dark_4_items", |b| {
        b.iter(|| render(black_box(&snapshot), &config, 96))
    });
}

criterion_group!(benches, frame);
criterion_main!(benches);
