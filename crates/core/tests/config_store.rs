use trafficview_core::config::{Config, ConfigStore};

#[test]
fn store_creates_then_loads_valid_config() {
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigStore::at(dir.path().join("config.toml"));
    let loaded = store.load_or_create().unwrap();
    assert_eq!(loaded, Config::default());
    assert!(store.path().is_file());
}

#[test]
fn store_does_not_overwrite_invalid_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "not = [valid").unwrap();
    let store = ConfigStore::at(path.clone());
    assert!(store.load_or_create().is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "not = [valid");
}

#[test]
fn save_rejects_invalid_config_without_touching_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let store = ConfigStore::at(path.clone());
    let cfg = Config {
        sample_interval_ms: 10,
        ..Config::default()
    };
    assert!(store.save(&cfg).is_err());
    assert!(!path.exists());
}

#[test]
fn save_round_trips_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigStore::at(dir.path().join("config.toml"));
    let cfg = Config {
        sample_interval_ms: 1500,
        overlay: trafficview_core::config::OverlayConfig {
            position: [10, 20],
            ..Default::default()
        },
        ..Config::default()
    };
    store.save(&cfg).unwrap();
    assert_eq!(store.load().unwrap(), cfg);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != "config.toml")
        .collect();
    assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
}
