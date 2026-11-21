use assert_cmd::Command;
use predicates::prelude::*;

fn cli(config_home: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("trafficview").unwrap();
    command.env("XDG_CONFIG_HOME", config_home);
    command
}

#[test]
fn autostart_enable_is_idempotent_and_disable_keeps_other_entries() {
    let dir = tempfile::tempdir().unwrap();
    cli(dir.path())
        .args(["autostart", "enable"])
        .assert()
        .success();
    let desktop = dir.path().join("autostart/trafficview.desktop");
    let first = std::fs::read_to_string(&desktop).unwrap();
    assert!(first.contains("tray --autostart"), "{first}");
    assert!(first.contains("X-GNOME-Autostart-enabled=true"), "{first}");
    assert!(first.contains("Exec="), "{first}");

    // 重复 enable 幂等：内容不变、仍成功。
    cli(dir.path())
        .args(["autostart", "enable"])
        .assert()
        .success()
        .stdout(predicate::str::contains("trafficview.desktop"));
    assert_eq!(std::fs::read_to_string(&desktop).unwrap(), first);

    let other = dir.path().join("autostart/other.desktop");
    std::fs::write(&other, "keep").unwrap();
    cli(dir.path())
        .args(["autostart", "disable"])
        .assert()
        .success();
    assert!(!desktop.exists());
    assert!(other.exists());
}

#[test]
fn autostart_status_reports_enabled_state() {
    let dir = tempfile::tempdir().unwrap();
    cli(dir.path())
        .args(["autostart", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("disabled"));
    cli(dir.path())
        .args(["autostart", "enable"])
        .assert()
        .success();
    cli(dir.path())
        .args(["autostart", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("enabled"));
}
