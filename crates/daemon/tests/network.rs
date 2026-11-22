//! 自动选卡策略：全部使用 fixture，不依赖本机网卡。

use std::collections::HashMap;
use trafficview_core::InterfaceInfo;
use trafficview_daemon::network::{NetSelector, SelectError};

fn iface(name: &str, operstate: &str) -> InterfaceInfo {
    InterfaceInfo {
        name: name.into(),
        ifindex: 1,
        stable: format!("mac:{}", name),
        mac: Some(name.to_ascii_lowercase()),
        operstate: operstate.into(),
        carrier: operstate == "up",
        is_loopback: name == "lo",
    }
}

fn rates(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
    pairs
        .iter()
        .map(|(name, rate)| ((*name).to_owned(), *rate))
        .collect()
}

fn selector(default_route: Option<&str>, whitelist: &[&str]) -> NetSelector {
    NetSelector::new(
        whitelist,
        &["lo", "docker*", "veth*", "br-*", "virbr*", "tun*"],
        default_route.map(str::to_owned),
    )
    .unwrap()
}

#[test]
fn default_route_wins_before_rate_based_selection() {
    let interfaces = vec![iface("eth0", "up"), iface("wlan0", "up")];
    let selector = selector(Some("eth0"), &[]);
    let chosen = selector
        .choose(&interfaces, &rates(&[("eth0", 100), ("wlan0", 9000)]))
        .unwrap();
    assert_eq!(chosen.name, "eth0");
}

#[test]
fn fastest_active_interface_is_used_without_default_route() {
    let interfaces = vec![
        iface("eth0", "up"),
        iface("wlan0", "up"),
        iface("lo", "unknown"),
    ];
    let selector = selector(None, &[]);
    let chosen = selector
        .choose(
            &interfaces,
            &rates(&[("eth0", 100), ("wlan0", 9000), ("lo", 99999)]),
        )
        .unwrap();
    assert_eq!(chosen.name, "wlan0");
}

#[test]
fn whitelist_overrides_blacklist_and_hotplug_rebases() {
    let interfaces = vec![iface("tun0", "up"), iface("eth0", "up")];
    let selector = selector(None, &["tun0"]);
    let chosen = selector
        .choose(&interfaces, &rates(&[("tun0", 50), ("eth0", 10)]))
        .unwrap();
    assert_eq!(chosen.name, "tun0");
}

#[test]
fn disappearing_interface_falls_back_to_the_remaining_one() {
    let selector = selector(Some("eth0"), &[]);
    let before = vec![iface("eth0", "up"), iface("wlan0", "up")];
    assert_eq!(
        selector
            .choose(&before, &rates(&[("eth0", 1)]))
            .unwrap()
            .name,
        "eth0"
    );
    let after = vec![iface("wlan0", "up")];
    assert_eq!(
        selector
            .choose(&after, &rates(&[("wlan0", 1)]))
            .unwrap()
            .name,
        "wlan0"
    );
}

#[test]
fn default_route_change_is_picked_up_on_the_next_selection() {
    let interfaces = vec![iface("eth0", "up"), iface("wlan0", "up")];
    let eth0 = selector(Some("eth0"), &[]);
    let wlan0 = selector(Some("wlan0"), &[]);
    assert_eq!(eth0.choose(&interfaces, &rates(&[])).unwrap().name, "eth0");
    assert_eq!(
        wlan0.choose(&interfaces, &rates(&[])).unwrap().name,
        "wlan0"
    );
}

#[test]
fn all_down_or_only_blacklisted_interfaces_report_no_active_interface() {
    let down = vec![iface("eth0", "down"), iface("wlan0", "down")];
    assert_eq!(
        selector(Some("eth0"), &[]).choose(&down, &rates(&[])),
        Err(SelectError::NoActiveInterface)
    );

    let blacklisted = vec![iface("docker0", "up"), iface("veth123", "up")];
    assert_eq!(
        selector(None, &[]).choose(&blacklisted, &rates(&[])),
        Err(SelectError::NoActiveInterface)
    );
}

#[test]
fn whitelist_only_helps_interfaces_that_are_up() {
    let interfaces = vec![iface("tun0", "down"), iface("eth0", "up")];
    let selector = selector(None, &["tun0"]);
    assert_eq!(
        selector
            .choose(&interfaces, &rates(&[("tun0", 1)]))
            .unwrap()
            .name,
        "eth0"
    );
}
