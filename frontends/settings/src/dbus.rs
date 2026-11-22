//! 独立 D-Bus 客户端：在后台 tokio 线程中维护与 daemon 的连接。
//!
//! - 连接失败/丢失时退避重连，并通过 `ClientEvent::Disconnected` 通知 UI 禁用控件。
//! - 初始拉取 `GetConfig`、`Interfaces`、`CollectorStatus`；之后监听 `ConfigChanged`
//!   信号刷新，使其它前端（CLI/tray）的修改在设置窗口中实时反映。
//! - 所有保存走 `UpdateConfig`，设置界面**绝不**直接写 daemon 配置文件。
//! - UI 与 worker 通过 `async_channel`（UI→worker）和 `glib::Sender`（worker→UI）解耦；
//!   GTK 控件只在主线程被修改。

use std::{collections::HashMap, time::Duration};

use futures_util::StreamExt;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, Proxy};

use crate::config_model::{from_dbus, to_update_dbus, UiConfig};

pub const BUS_NAME: &str = "com.trafficview.Daemon1";
pub const PATH: &str = "/com/trafficview/Daemon1";
pub const INTERFACE: &str = "com.trafficview.Daemon1";

/// UI 发往后台 worker 的命令。
pub enum DbusCommand {
    /// 重新拉取配置与采集器状态。
    Refresh,
    /// 提交一次完整的 `UpdateConfig`。
    UpdateConfig(HashMap<String, OwnedValue>),
    /// 退出 worker 线程。
    Quit,
}

/// worker 发往 UI 主线程的事件。
/// 各变体大小差异较大，但事件通道吞吐低，允许该 lint。
#[allow(clippy::large_enum_variant)]
pub enum ClientEvent {
    Connected,
    Disconnected(String),
    Config(UiConfig),
    Interfaces(Vec<String>),
    CollectorStatus(HashMap<String, OwnedValue>),
    /// `UpdateConfig` 的结果：成功为空，失败为 daemon 返回的可展示原因。
    UpdateResult(Result<(), String>),
}

/// 持有命令通道；drop 时通知 worker 退出。
pub struct DaemonClient {
    command_tx: async_channel::Sender<DbusCommand>,
}

impl DaemonClient {
    /// 启动后台线程；`event_tx` 用于把事件送回 GTK 主上下文。
    pub fn start(event_tx: async_channel::Sender<ClientEvent>) -> Self {
        let (command_tx, command_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("trafficview-settings-dbus".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(error) => {
                        let _ = event_tx.try_send(ClientEvent::Disconnected(format!(
                            "cannot start runtime: {error}"
                        )));
                        return;
                    }
                };
                runtime.block_on(worker_loop(command_rx, event_tx));
            })
            .expect("spawn dbus worker thread");
        Self { command_tx }
    }

    pub fn refresh(&self) {
        let _ = self.command_tx.try_send(DbusCommand::Refresh);
    }

    pub fn update_config(&self, cfg: &UiConfig) {
        let map = to_update_dbus(cfg);
        let _ = self.command_tx.try_send(DbusCommand::UpdateConfig(map));
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(DbusCommand::Quit);
    }
}

enum RunOutcome {
    Quit,
    Disconnected(String),
}

async fn worker_loop(
    cmd_rx: async_channel::Receiver<DbusCommand>,
    event_tx: async_channel::Sender<ClientEvent>,
) {
    loop {
        match run_session(&cmd_rx, &event_tx).await {
            RunOutcome::Quit => break,
            RunOutcome::Disconnected(reason) => {
                let _ = event_tx.try_send(ClientEvent::Disconnected(reason));
                // 退避重连；退避期间仍可响应 Quit 立即退出。
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    cmd = cmd_rx.recv() => {
                        if let Ok(DbusCommand::Quit) = cmd {
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn run_session(
    cmd_rx: &async_channel::Receiver<DbusCommand>,
    event_tx: &async_channel::Sender<ClientEvent>,
) -> RunOutcome {
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(error) => {
            return RunOutcome::Disconnected(format!("cannot connect to session bus: {error}"));
        }
    };
    let proxy = match Proxy::new(&conn, BUS_NAME, PATH, INTERFACE).await {
        Ok(p) => p,
        Err(error) => {
            return RunOutcome::Disconnected(format!("cannot build D-Bus proxy: {error}"));
        }
    };

    let raw_config = match fetch_config(&proxy).await {
        Ok(cfg) => cfg,
        Err(error) => return RunOutcome::Disconnected(error),
    };
    let _ = event_tx.try_send(ClientEvent::Connected);
    let _ = event_tx.try_send(ClientEvent::Config(from_dbus(&raw_config)));

    if let Ok(interfaces) = fetch_interfaces(&proxy).await {
        let _ = event_tx.try_send(ClientEvent::Interfaces(interfaces));
    }
    if let Ok(status) = fetch_collector_status(&proxy).await {
        let _ = event_tx.try_send(ClientEvent::CollectorStatus(status));
    }

    let mut changes = match proxy.receive_signal("ConfigChanged").await {
        Ok(stream) => stream,
        Err(error) => {
            return RunOutcome::Disconnected(format!("cannot subscribe ConfigChanged: {error}"));
        }
    };

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Ok(DbusCommand::Quit) => return RunOutcome::Quit,
                    Ok(DbusCommand::Refresh) => {
                        if let Ok(raw) = fetch_config(&proxy).await {
                            let _ = event_tx.try_send(ClientEvent::Config(from_dbus(&raw)));
                        } else {
                            return RunOutcome::Disconnected("GetConfig failed".into());
                        }
                        if let Ok(status) = fetch_collector_status(&proxy).await {
                            let _ = event_tx.try_send(ClientEvent::CollectorStatus(status));
                        }
                    }
                    Ok(DbusCommand::UpdateConfig(map)) => {
                        let result = update_config(&proxy, map).await;
                        let _ = event_tx.try_send(ClientEvent::UpdateResult(result));
                    }
                    Err(_) => return RunOutcome::Quit,
                }
            }
            signal = changes.next() => {
                match signal {
                    Some(_) => {
                        if let Ok(raw) = fetch_config(&proxy).await {
                            let _ = event_tx.try_send(ClientEvent::Config(from_dbus(&raw)));
                        } else {
                            return RunOutcome::Disconnected("GetConfig after ConfigChanged failed".into());
                        }
                        if let Ok(status) = fetch_collector_status(&proxy).await {
                            let _ = event_tx.try_send(ClientEvent::CollectorStatus(status));
                        }
                    }
                    None => return RunOutcome::Disconnected("ConfigChanged stream ended".into()),
                }
            }
        }
    }
}

async fn fetch_config(proxy: &Proxy<'_>) -> Result<HashMap<String, OwnedValue>, String> {
    let reply = proxy
        .call_method("GetConfig", &())
        .await
        .map_err(|error| format!("GetConfig failed: {error}"))?;
    reply
        .body()
        .deserialize::<HashMap<String, OwnedValue>>()
        .map_err(|error| format!("GetConfig decode: {error}"))
}

async fn fetch_interfaces(proxy: &Proxy<'_>) -> Result<Vec<String>, String> {
    proxy
        .get_property::<Vec<String>>("Interfaces")
        .await
        .map_err(|error| format!("Interfaces property: {error}"))
}

async fn fetch_collector_status(proxy: &Proxy<'_>) -> Result<HashMap<String, OwnedValue>, String> {
    proxy
        .get_property::<HashMap<String, OwnedValue>>("CollectorStatus")
        .await
        .map_err(|error| format!("CollectorStatus property: {error}"))
}

async fn update_config(proxy: &Proxy<'_>, map: HashMap<String, OwnedValue>) -> Result<(), String> {
    proxy
        .call_method("UpdateConfig", &(map,))
        .await
        .map_err(|error| format!("UpdateConfig failed: {error}"))?;
    Ok(())
}
