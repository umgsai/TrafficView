//! 格式化路径基准：输入固定，不访问 `/proc`、时钟或 D-Bus，结果可重复。

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use trafficview_core::format::{format_bit_rate, format_bytes, format_rate};

const RATES: [u64; 6] = [0, 999, 12_345, 1_234_567, 987_654_321, 12_345_678_901];
const TOTALS: [u64; 6] = [
    0,
    1023,
    1_048_576,
    5_368_709_120,
    1_099_511_627_776,
    9_223_372_036_854,
];

fn format(c: &mut Criterion) {
    c.bench_function("format/rate_bytes", |b| {
        b.iter(|| {
            for value in RATES {
                black_box(format_rate(value));
            }
        })
    });
    c.bench_function("format/rate_bits", |b| {
        b.iter(|| {
            for value in RATES {
                black_box(format_bit_rate(value));
            }
        })
    });
    c.bench_function("format/bytes", |b| {
        b.iter(|| {
            for value in TOTALS {
                black_box(format_bytes(value));
            }
        })
    });
}

criterion_group!(benches, format);
criterion_main!(benches);
