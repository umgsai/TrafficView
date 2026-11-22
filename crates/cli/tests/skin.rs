use std::{fs, path::PathBuf};

use assert_cmd::Command;
use tempfile::tempdir;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../render/tests/fixtures/minimal")
}

fn snapshot_json() -> serde_json::Value {
    serde_json::json!({
        "ts_ms": 1,
        "net": {"iface": "eth0", "rx_bps": 1024, "tx_bps": 2048, "rx_total": 0, "tx_total": 0},
        "cpu": {"total_pct": 12.0, "per_core": [12.0], "freq_mhz": null, "load": null},
        "mem": {"used_pct": 34.0, "total": 100, "available": 66, "swap_pct": 0.0},
        "disk": null,
        "gpu": null,
        "temps": [],
        "plugin": {}
    })
}

#[test]
fn skin_validate_prints_normalized_summary() {
    let output = Command::cargo_bin("trafficview")
        .unwrap()
        .args(["skin", "validate"])
        .arg(fixture())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["meta"]["name"], "Minimal");
    assert_eq!(value["canvas"]["width"], 220);
    assert_eq!(value["items"].as_array().unwrap().len(), 2);
}

#[test]
fn skin_preview_writes_png_without_a_graphical_session() {
    let directory = tempdir().unwrap();
    let snapshot = directory.path().join("snapshot.json");
    fs::write(&snapshot, snapshot_json().to_string()).unwrap();
    let preview = directory.path().join("preview.png");
    let output = Command::cargo_bin("trafficview")
        .unwrap()
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .args(["skin", "preview"])
        .arg(fixture())
        .arg("--snapshot")
        .arg(&snapshot)
        .arg("--output")
        .arg(&preview)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let png = fs::read(&preview).unwrap();
    assert_eq!(&png[1..4], b"PNG");
    assert!(png.len() > 100);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["meta"]["name"], "Minimal");
}

#[test]
fn skin_validate_rejects_background_outside_skin_directory() {
    let directory = tempdir().unwrap();
    fs::write(
        directory.path().join("skin.toml"),
        "[meta]\nname='bad'\n[canvas]\nwidth=100\nheight=40\nbackground='../../secret.png'\n[[item]]\nid='cpu'\nrect=[0,0,40,20]\n",
    )
    .unwrap();
    let output = Command::cargo_bin("trafficview")
        .unwrap()
        .args(["skin", "validate"])
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("outside skin directory"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
