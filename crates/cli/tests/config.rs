use assert_cmd::Command;
use predicates::prelude::*;

fn cli(config_home: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("trafficview").unwrap();
    command.env("XDG_CONFIG_HOME", config_home);
    command
}

#[test]
fn config_set_and_get_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    cli(dir.path())
        .args(["config", "set", "sample_interval_ms", "1500"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sample_interval_ms = 1500"));

    cli(dir.path())
        .args(["config", "get"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sample_interval_ms = 1500"));
}

#[test]
fn config_set_rejects_unknown_key_and_invalid_value() {
    let dir = tempfile::tempdir().unwrap();
    cli(dir.path())
        .args(["config", "set", "nope", "1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported config key"));

    cli(dir.path())
        .args(["config", "set", "sample_interval_ms", "100"])
        .assert()
        .failure();

    let text = std::fs::read_to_string(dir.path().join("trafficview/config.toml")).unwrap();
    assert!(text.contains("sample_interval_ms = 1000"), "{text}");
}

#[test]
fn config_path_points_into_xdg_home() {
    let dir = tempfile::tempdir().unwrap();
    cli(dir.path())
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("trafficview/config.toml"));
}
