use trafficview_cli::{format_disconnected, format_polybar, format_tmux, format_waybar};
use trafficview_core::{CpuSample, MemSample, NetSample, Snapshot};

fn test_snapshot() -> Snapshot {
    Snapshot::new(
        42,
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
            total: 100,
            available: 66,
            swap_pct: 1.0,
        },
    )
}

#[test]
fn waybar_output_is_single_line_json_with_tooltip_and_state() {
    let output = format_waybar(&test_snapshot(), false).unwrap();
    assert!(!output.contains('\n'));
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["class"], "normal");
    assert!(value["text"].as_str().unwrap().contains("↓"));
    assert!(value["tooltip"].as_str().unwrap().contains("CPU"));
}

#[test]
fn waybar_bits_mode_uses_binary_bit_prefixes() {
    let output = format_waybar(&test_snapshot(), true).unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    let text = value["text"].as_str().unwrap();
    assert!(text.contains("8.0 Kb/s"), "{text}");
    assert!(text.contains("16.0 Kb/s"), "{text}");
}

#[test]
fn disconnected_output_is_explicit() {
    let value: serde_json::Value = serde_json::from_str(&format_disconnected().unwrap()).unwrap();
    assert_eq!(value["class"], "disconnected");
    assert_eq!(value["text"], "TrafficView: --");
}

#[test]
fn scriptable_formats_stay_single_line_and_control_free() {
    for output in [
        format_polybar(&test_snapshot(), false),
        format_tmux(&test_snapshot(), false),
    ] {
        assert!(!output.contains('\n'));
        assert!(!output.chars().any(char::is_control), "{output:?}");
        assert!(output.contains("CPU"));
    }
}
