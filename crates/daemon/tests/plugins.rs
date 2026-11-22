//! 插件宿主测试：全部使用 `/bin/sh` fixture，不依赖真实插件或用户数据。

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use trafficview_daemon::plugins::{
    discover, run_plugin_once, FailureState, PluginManifest, PluginRunError, PluginStatus,
    PluginSupervisor, RuntimeConfig, MAX_CONSECUTIVE_FAILURES, PLUGIN_TIMEOUT,
};

fn sh_manifest(id: &str, script: &str, interval_ms: u32) -> PluginManifest {
    PluginManifest {
        id: id.into(),
        command: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        interval_ms,
        capabilities: vec!["metrics".into()],
    }
}

fn write_manifest(root: &Path, dir: &str, body: &str) {
    let path = root.join(dir).join("plugin.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn fast_runtime() -> RuntimeConfig {
    RuntimeConfig {
        timeout: Duration::from_millis(500),
        backoff: vec![Duration::from_millis(10); 7],
        max_failures: MAX_CONSECUTIVE_FAILURES,
        poll_interval: Duration::from_millis(10),
    }
}

#[test]
fn normal_plugin_publishes_qualified_values() {
    let manifest = sh_manifest(
        "demo",
        r#"read line; printf '%s\n' '{"items":[{"id":"fan_rpm","name":"FAN","unit":"rpm","format":"{value} rpm"}],"values":{"fan_rpm":"1420"}}'"#,
        1000,
    );
    let output = run_plugin_once(&manifest, 1.0, PLUGIN_TIMEOUT).unwrap();
    assert_eq!(
        output.values.get("demo.fan_rpm").map(String::as_str),
        Some("1420")
    );
}

#[test]
fn timeout_kills_the_plugin_and_default_limit_is_two_seconds() {
    assert_eq!(PLUGIN_TIMEOUT, Duration::from_secs(2));
    let manifest = sh_manifest("slow", "sleep 5", 1000);
    let error = run_plugin_once(&manifest, 1.0, Duration::from_millis(300)).unwrap_err();
    assert!(matches!(error, PluginRunError::Timeout), "{error}");
}

#[test]
fn immediate_crash_reports_exit_status() {
    let manifest = sh_manifest("crash", "exit 3", 1000);
    let error = run_plugin_once(&manifest, 1.0, PLUGIN_TIMEOUT).unwrap_err();
    assert!(
        matches!(error, PluginRunError::Exit(Some(3))),
        "unexpected error: {error}"
    );
}

#[test]
fn stderr_flood_is_drained_without_deadlock() {
    let manifest = sh_manifest(
        "noisy",
        r#"i=0; while [ "$i" -lt 3000 ]; do echo "stderr noise stderr noise stderr noise" >&2; i=$((i+1)); done; read line; printf '%s\n' '{"items":[{"id":"ok","name":"OK","unit":"","format":"{value}"}],"values":{"ok":"1"}}'"#,
        1000,
    );
    let output = run_plugin_once(&manifest, 1.0, Duration::from_secs(5)).unwrap();
    assert_eq!(output.values.get("noisy.ok").map(String::as_str), Some("1"));
}

#[test]
fn three_consecutive_failures_disable_until_reset() {
    let mut state = FailureState::default();
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        assert!(!state.is_disabled());
        state.record_failure(MAX_CONSECUTIVE_FAILURES);
    }
    assert!(state.is_disabled());
    assert_eq!(state.consecutive(), MAX_CONSECUTIVE_FAILURES);

    let mut state = FailureState::default();
    state.record_failure(MAX_CONSECUTIVE_FAILURES);
    state.record_failure(MAX_CONSECUTIVE_FAILURES);
    state.record_success();
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        state.record_failure(MAX_CONSECUTIVE_FAILURES);
    }
    assert!(state.is_disabled());

    let mut state = FailureState::default();
    for _ in 0..MAX_CONSECUTIVE_FAILURES {
        state.record_failure(MAX_CONSECUTIVE_FAILURES);
    }
    state.record_success();
    assert!(!state.is_disabled());
    assert_eq!(state.consecutive(), 0);
}

#[test]
fn backoff_follows_exponential_schedule_and_saturates() {
    let schedule: Vec<Duration> = [1, 2, 4, 8, 16, 32, 60]
        .into_iter()
        .map(Duration::from_secs)
        .collect();
    let mut state = FailureState::default();
    let delays: Vec<u64> = (0..9)
        .map(|_| {
            state.record_failure(99);
            state.backoff(&schedule).as_secs()
        })
        .collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
}

#[test]
fn discovery_rejects_invalid_manifests_but_keeps_valid_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("plugins");
    write_manifest(
        &root,
        "good",
        "id = \"good\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"exit 0\"]\ninterval_ms = 1000\ncapabilities = [\"metrics\"]\n",
    );
    write_manifest(
        &root,
        "relative",
        "id = \"relative\"\ncommand = \"plugin\"\ninterval_ms = 1000\n",
    );
    write_manifest(
        &root,
        "mismatch",
        "id = \"other\"\ncommand = \"/bin/sh\"\ninterval_ms = 1000\n",
    );
    write_manifest(
        &root,
        "fast",
        "id = \"fast\"\ncommand = \"/bin/sh\"\ninterval_ms = 100\n",
    );

    let found = discover(&root);
    let ids: Vec<&str> = found.manifests.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["good"]);
    assert_eq!(found.errors.len(), 3, "{:?}", found.errors);
    assert!(found.errors.iter().any(|error| error.contains("absolute")));
    assert!(found
        .errors
        .iter()
        .any(|error| error.contains("does not match")));
    assert!(found
        .errors
        .iter()
        .any(|error| error.contains("interval_ms")));
}

#[test]
fn supervisor_disables_after_failures_and_reload_retries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("plugins");
    let marker = dir.path().join("ready");
    let script = write_script(
        dir.path(),
        "demo.sh",
        "if [ ! -f \"$1\" ]; then exit 1; fi\nread line\nprintf '%s\\n' '{\"items\":[{\"id\":\"ok\",\"name\":\"OK\",\"unit\":\"\",\"format\":\"{value}\"}],\"values\":{\"ok\":\"1\"}}'\n",
    );
    write_manifest(
        &root,
        "demo",
        &format!(
            "id = \"demo\"\ncommand = \"/bin/sh\"\nargs = [\"{}\", \"{}\"]\ninterval_ms = 500\n",
            script.display(),
            marker.display()
        ),
    );

    let supervisor = PluginSupervisor::start(&root, fast_runtime(), true);
    let disabled: Option<PluginStatus> = wait_until(|| {
        supervisor
            .statuses()
            .get("demo")
            .is_some_and(|status| !status.available && status.reason.contains("disabled"))
    })
    .then(|| supervisor.statuses().get("demo").cloned())
    .flatten();
    assert!(
        disabled.is_some(),
        "plugin should be disabled after failures"
    );
    assert!(supervisor.published_values().is_empty());

    fs::write(&marker, "1").unwrap();
    supervisor.reload(true);
    assert!(
        wait_until(|| supervisor.published_values().contains_key("demo.ok")),
        "reload should allow the plugin to retry"
    );
    supervisor.stop();
}

#[test]
fn supervisor_callers_are_never_blocked_by_slow_plugins() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("plugins");
    write_manifest(
        &root,
        "slow",
        "id = \"slow\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"sleep 5\"]\ninterval_ms = 500\n",
    );

    let supervisor = PluginSupervisor::start(&root, fast_runtime(), true);
    let started = Instant::now();
    for _ in 0..10 {
        let _ = supervisor.published_values();
        let _ = supervisor.statuses();
    }
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "supervisor queries must not wait for plugins"
    );
    supervisor.stop();
}
