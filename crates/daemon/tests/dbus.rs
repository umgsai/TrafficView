use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use tokio::{sync::Mutex, task::JoinHandle, time::timeout};
use trafficview_core::config::ConfigStore;
use zbus::zvariant::{OwnedValue, Value};

/// 两个测试共用同一个 session bus 名称，必须串行。
static BUS_LOCK: Mutex<()> = Mutex::const_new(());

#[zbus::proxy(
    interface = "com.trafficview.Daemon1",
    default_service = "com.trafficview.Daemon1",
    default_path = "/com/trafficview/Daemon1"
)]
trait Daemon1 {
    fn get_snapshot(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    fn get_traffic_summary(
        &self,
        scope: &str,
        iface_stable: &str,
    ) -> zbus::Result<HashMap<String, OwnedValue>>;
    fn get_interface_details(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    fn get_config(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    fn update_config(&self, updates: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    fn set_active_interface(&self, name: &str) -> zbus::Result<()>;
    fn set_interval(&self, ms: u32) -> zbus::Result<()>;
    fn pause(&self, paused: bool) -> zbus::Result<()>;
    fn reload_config(&self) -> zbus::Result<()>;
    fn set_overlay_visible(&self, visible: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn sample_interval(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn active_interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn collector_status(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(signal)]
    fn sample(&self, snapshot: HashMap<String, OwnedValue>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn config_changed(&self) -> zbus::Result<()>;
}

struct TestDaemon {
    _dir: tempfile::TempDir,
    handle: JoinHandle<()>,
    connection: zbus::Connection,
}

impl TestDaemon {
    async fn spawn() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let connection = zbus::Connection::session().await?;
        // 上一个测试的 daemon 可能还没被运行时回收；等待名称释放，避免误连旧实例。
        let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
        let bus_name = zbus::names::BusName::try_from("com.trafficview.Daemon1")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while match dbus.get_name_owner(bus_name.clone()).await {
            Ok(_) => true,
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => false,
            Err(error) => return Err(error.into()),
        } {
            if Instant::now() >= deadline {
                anyhow::bail!("bus name is still owned by a previous test daemon");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let store = ConfigStore::at(dir.path().join("config.toml"));
        store
            .load_or_create()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let history_path = dir.path().join("history.db");
        // 上一个测试 abort 掉 daemon 后，总线名称释放是异步的；先等它消失，
        // 否则新 daemon 会因 NameTaken 提前退出，等待循环只能拿到超时错误。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let reply = connection
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &("com.trafficview.Daemon1",),
                )
                .await?;
            let owned: bool = reply.body().deserialize()?;
            if !owned {
                break;
            }
            if Instant::now() >= deadline {
                anyhow::bail!("previous trafficviewd did not release its bus name");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let handle = tokio::spawn(async move {
            if let Err(error) =
                trafficview_daemon::service::run_with_history(store, Some(history_path)).await
            {
                eprintln!("daemon exited: {error}");
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let proxy = Daemon1Proxy::new(&connection).await?;
            match proxy.get_snapshot().await {
                Ok(_) => break,
                Err(error) => {
                    if Instant::now() >= deadline {
                        return Err(error.into());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        Ok(Self {
            _dir: dir,
            handle,
            connection,
        })
    }

    async fn proxy(&self) -> zbus::Result<Daemon1Proxy<'_>> {
        Daemon1Proxy::new(&self.connection).await
    }

    fn config_path(&self) -> std::path::PathBuf {
        self._dir.path().join("config.toml")
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn net_dict(snapshot: &HashMap<String, OwnedValue>) -> HashMap<String, OwnedValue> {
    snapshot["net"]
        .try_clone()
        .expect("net is owned")
        .try_into()
        .expect("net is a{sv}")
}

fn u32_value(value: u32) -> OwnedValue {
    OwnedValue::try_from(Value::from(value)).unwrap()
}

fn bool_value(value: bool) -> OwnedValue {
    OwnedValue::try_from(Value::from(value)).unwrap()
}

fn plugins_value(enabled: bool) -> OwnedValue {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("enabled".into(), Value::from(enabled));
    OwnedValue::try_from(Value::from(map)).unwrap()
}

fn history_value(days: u32) -> OwnedValue {
    let mut map: HashMap<String, Value<'static>> = HashMap::new();
    map.insert("retention_days".into(), Value::from(days));
    OwnedValue::try_from(Value::from(map)).unwrap()
}

#[tokio::test]
async fn config_api_round_trips_and_rejects_invalid_updates_atomically() {
    let _guard = BUS_LOCK.lock().await;
    let harness = TestDaemon::spawn().await.unwrap();
    let proxy = harness.proxy().await.unwrap();

    let config = proxy.get_config().await.unwrap();
    assert_eq!(u32::try_from(&config["sample_interval_ms"]).unwrap(), 1000);
    assert!(config.contains_key("overlay"));
    assert!(config.contains_key("alerts"));
    assert!(config.contains_key("history"));
    assert!(config.contains_key("process_traffic"));

    proxy
        .update_config(HashMap::from([
            ("sample_interval_ms".into(), u32_value(1500)),
            ("memory_include_cache".into(), bool_value(true)),
        ]))
        .await
        .unwrap();
    let after = proxy.get_config().await.unwrap();
    assert_eq!(u32::try_from(&after["sample_interval_ms"]).unwrap(), 1500);
    assert!(bool::try_from(&after["memory_include_cache"]).unwrap());

    // 同一批更新里既有合法也有非法字段：整个事务必须回滚。
    let error = proxy
        .update_config(HashMap::from([
            ("sample_interval_ms".into(), u32_value(100)),
            ("plugins".into(), plugins_value(false)),
        ]))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("org.freedesktop.DBus.Error.InvalidArgs"),
        "unexpected error: {error}"
    );
    let after_error = proxy.get_config().await.unwrap();
    assert_eq!(
        u32::try_from(&after_error["sample_interval_ms"]).unwrap(),
        1500
    );
    let plugins: HashMap<String, OwnedValue> = after_error["plugins"]
        .try_clone()
        .unwrap()
        .try_into()
        .unwrap();
    assert!(
        bool::try_from(&plugins["enabled"]).unwrap(),
        "invalid batch must not flip plugins.enabled"
    );

    // 磁盘上的配置同样没有半写状态。
    let saved = ConfigStore::at(harness.config_path()).load().unwrap();
    assert_eq!(saved.sample_interval_ms, 1500);
    assert!(saved.memory_include_cache);
    assert!(saved.plugins.enabled);

    assert!(
        proxy
            .update_config(HashMap::from([("unknown_key".into(), u32_value(1))]))
            .await
            .is_err(),
        "unknown keys must be rejected"
    );
}

#[tokio::test]
async fn concurrent_config_updates_do_not_lose_fields() {
    let _guard = BUS_LOCK.lock().await;
    let harness = TestDaemon::spawn().await.unwrap();
    let proxy_a = harness.proxy().await.unwrap();
    let connection_b = zbus::Connection::session().await.unwrap();
    let proxy_b = Daemon1Proxy::new(&connection_b).await.unwrap();

    let (first, second) = tokio::join!(
        proxy_a.update_config(HashMap::from([(
            "memory_include_cache".into(),
            bool_value(true),
        )])),
        proxy_b.update_config(HashMap::from([("history".into(), history_value(30))])),
    );
    first.unwrap();
    second.unwrap();

    let config = proxy_a.get_config().await.unwrap();
    assert!(bool::try_from(&config["memory_include_cache"]).unwrap());
    let history: HashMap<String, OwnedValue> =
        config["history"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(u32::try_from(&history["retention_days"]).unwrap(), 30);

    let saved = ConfigStore::at(harness.config_path()).load().unwrap();
    assert!(saved.memory_include_cache);
    assert_eq!(saved.history.retention_days, 30);
}

#[tokio::test]
async fn snapshot_properties_and_interval_validation_are_exposed() {
    let _guard = BUS_LOCK.lock().await;
    let harness = TestDaemon::spawn().await.unwrap();
    let proxy = harness.proxy().await.unwrap();

    let snapshot = proxy.get_snapshot().await.unwrap();
    for key in ["ts_ms", "net", "cpu", "mem"] {
        assert!(snapshot.contains_key(key), "missing {key}");
    }
    assert_eq!(snapshot["net"].value_signature().to_string(), "a{sv}");
    let net = net_dict(&snapshot);
    for key in ["iface", "rx_bps", "tx_bps", "rx_total", "tx_total"] {
        assert!(net.contains_key(key), "missing net.{key}");
    }
    // M4 起 disk 是可选键：存在时必须是 a{sv}，不再要求缺失。
    if let Some(disk) = snapshot.get("disk") {
        assert_eq!(disk.value_signature().to_string(), "a{sv}");
    }
    // M4 起 gpu/gpus 是可选键；本机无受支持后端时缺省，存在时必须签名正确。
    if let Some(gpu) = snapshot.get("gpu") {
        assert_eq!(gpu.value_signature().to_string(), "a{sv}");
    }
    if let Some(gpus) = snapshot.get("gpus") {
        assert_eq!(gpus.value_signature().to_string(), "av");
    }
    assert_eq!(
        snapshot["temps"].value_signature().to_string(),
        "av",
        "temps must be an empty array of variants"
    );
    assert_eq!(
        snapshot["plugin"].value_signature().to_string(),
        "a{ss}",
        "plugin must be an empty string map"
    );
    assert!(
        !snapshot.contains_key("processes"),
        "processes must be omitted while the backend is off"
    );

    let status = harness
        .proxy()
        .await
        .unwrap()
        .collector_status()
        .await
        .unwrap();
    let process_status: HashMap<String, OwnedValue> =
        status["processes"].try_clone().unwrap().try_into().unwrap();
    assert!(!bool::try_from(&process_status["available"]).unwrap());
    let reason = String::try_from(process_status["reason"].try_clone().unwrap()).unwrap();
    assert!(reason.contains("off"), "unexpected reason: {reason}");

    assert_eq!(proxy.sample_interval().await.unwrap(), 1000);
    assert!(
        proxy
            .inner()
            .call::<_, _, ()>("SetInterval", &(100u32))
            .await
            .is_err(),
        "out-of-range interval must be rejected"
    );
    proxy
        .inner()
        .call::<_, _, ()>("SetInterval", &(1500u32))
        .await
        .unwrap();
    // zbus 的 Proxy 会缓存属性值；用新 proxy 读取真实状态。
    let fresh = harness.proxy().await.unwrap();
    assert_eq!(fresh.sample_interval().await.unwrap(), 1500);
}

#[tokio::test]
async fn traffic_summary_and_interface_details_are_exposed() {
    let _guard = BUS_LOCK.lock().await;
    let harness = TestDaemon::spawn().await.unwrap();
    let proxy = harness.proxy().await.unwrap();

    let summary = proxy.get_traffic_summary("day", "").await.unwrap();
    for key in ["scope", "start", "end", "rx", "tx", "interfaces"] {
        assert!(summary.contains_key(key), "missing summary key {key}");
    }
    assert_eq!(summary["interfaces"].value_signature().to_string(), "av");
    assert!(
        proxy.get_traffic_summary("year", "").await.is_err(),
        "invalid scope must be rejected instead of returning all history"
    );

    let details = proxy.get_interface_details().await.unwrap();
    assert!(!details.is_empty(), "expected at least one interface");
    for (name, value) in &details {
        assert_eq!(value.value_signature().to_string(), "a{sv}", "{name}");
        let entry: HashMap<String, OwnedValue> = value.try_clone().unwrap().try_into().unwrap();
        for key in [
            "stable",
            "mac",
            "ifindex",
            "operstate",
            "carrier",
            "loopback",
            "active",
        ] {
            assert!(entry.contains_key(key), "missing {name}.{key}");
        }
    }
}

#[tokio::test]
async fn overlay_visibility_change_notifies_frontends() {
    let _guard = BUS_LOCK.lock().await;
    let harness = TestDaemon::spawn().await.unwrap();
    let proxy = harness.proxy().await.unwrap();
    let mut stream = proxy.receive_config_changed().await.unwrap();
    proxy.set_overlay_visible(false).await.unwrap();
    timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("ConfigChanged signal timed out")
        .expect("signal stream ended");
}

#[tokio::test]
async fn sample_signal_is_monotonic_and_missing_interface_is_rejected() {
    let _guard = BUS_LOCK.lock().await;
    let harness = TestDaemon::spawn().await.unwrap();
    let proxy = harness.proxy().await.unwrap();
    let interfaces = proxy.interfaces().await.unwrap();
    assert!(!interfaces.is_empty());
    assert_eq!(proxy.active_interface().await.unwrap(), "");

    let error = proxy.set_active_interface("missing0").await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("org.freedesktop.DBus.Error.Failed"),
        "unexpected error: {error}"
    );
    assert_eq!(proxy.active_interface().await.unwrap(), "");

    let mut stream = proxy.receive_sample().await.unwrap();
    let mut stamps = Vec::new();
    for _ in 0..2 {
        let signal = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("sample signal timed out")
            .expect("signal stream ended");
        let args = signal.args().unwrap();
        assert!(args.snapshot.contains_key("cpu"));
        stamps.push(u64::try_from(&args.snapshot["ts_ms"]).unwrap());
    }
    assert!(stamps[1] > stamps[0], "ts_ms must increase: {stamps:?}");
}
