use assert_cmd::Command;
use predicates::str::contains;
use std::{fs, time::SystemTime};

#[test]
fn sample_requires_an_explicit_interface_in_m0() {
    Command::cargo_bin("trafficview")
        .unwrap()
        .args(["sample", "--help"])
        .assert()
        .success()
        .stdout(contains("--interface <INTERFACE>"))
        .stdout(contains("--interval-ms <INTERVAL_MS>"))
        .stdout(contains("--count <COUNT>"));
}

fn first_non_loopback_interface() -> Option<String> {
    let table = fs::read_to_string("/proc/net/dev").ok()?;
    table
        .lines()
        .skip(2)
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim().to_owned()))
        .find(|name| name != "lo")
}

#[test]
fn ts_ms_is_absolute_monotonic_milliseconds() {
    let Some(iface) = first_non_loopback_interface() else {
        return;
    };
    let output = Command::cargo_bin("trafficview")
        .unwrap()
        .args([
            "sample",
            "--interface",
            &iface,
            "--count",
            "2",
            "--interval-ms",
            "500",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let rows: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    let (first, second) = (
        rows[0]["ts_ms"].as_u64().unwrap(),
        rows[1]["ts_ms"].as_u64().unwrap(),
    );
    assert!(second > first, "ts_ms must increase: {first} -> {second}");
    assert!(
        (400..=2500).contains(&(second - first)),
        "unexpected ts_ms delta: {}",
        second - first
    );
    // A process-relative counter restarts near zero; CLOCK_MONOTONIC is machine uptime.
    assert!(
        first > 60_000,
        "ts_ms must not restart per process: {first}"
    );
    let wall = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(first < wall, "ts_ms must not follow the wall clock");
}
