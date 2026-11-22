//! 历史存储基准：内存 SQLite，90 天、每 5 分钟一条（25920 行），与真实库结构一致。

use chrono::{Duration, TimeZone, Utc};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use trafficview_daemon::history::HistoryStore;

const DAYS: i64 = 90;
const STEP_MINUTES: i64 = 5;

fn fixture() -> (HistoryStore, i64) {
    let mut store = HistoryStore::open_memory().expect("in-memory history");
    let id = store
        .upsert_interface("mac:bench", "eth0")
        .expect("upsert interface");
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    for step in 0..(DAYS * 24 * 60 / STEP_MINUTES) {
        let at = start + Duration::minutes(step * STEP_MINUTES);
        store
            .add_delta(id, at, 1_000_000, 500_000)
            .expect("add delta");
    }
    (store, id)
}

fn queries(c: &mut Criterion) {
    let (store, id) = fixture();
    c.bench_function("history/minute_lookup", |b| {
        b.iter(|| store.minute(black_box(id), black_box("2026-02-15T12:05Z")))
    });
    c.bench_function("history/day_lookup", |b| {
        b.iter(|| store.day(black_box(id), black_box("2026-02-15")))
    });
}

criterion_group!(benches, queries);
criterion_main!(benches);
