use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use tokio::{sync::Mutex, task::JoinHandle, time::timeout};
use trafficview_core::config::ConfigStore;
use zbus::zvariant::OwnedValue;

/// 两个测试共用同一个 session bus 名称，必须串行。
static BUS_LOCK: Mutex<()> = Mutex::const_new(());

#[zbus::proxy(
    interface = "com.trafficview.Daemon1",
    default_service = "com.trafficview.Daemon1",
    default_path = "/com/trafficview/Daemon1"
)]
trait Daemon1 {
    fn get_snapshot(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
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
        let handle = tokio::spawn(async move {
            if let Err(error) = trafficview_daemon::service::run(store).await {
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
    assert!(!snapshot.contains_key("disk"), "disk must be omitted in M1");
    assert!(!snapshot.contains_key("gpu"), "gpu must be omitted in M1");
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
