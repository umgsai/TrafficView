use assert_cmd::Command;
use predicates::str::contains;

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
