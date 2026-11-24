//! StatusNotifierItem + com.canonical.dbusmenu，纯 Rust（zbus）实现，仅消费 D-Bus。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use trafficview_core::{config::ConfigStore, format::format_bytes, format::format_rate, Snapshot};
use trafficview_render::tray_icon;
use zbus::{
    object_server::SignalContext,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const BUS_NAME: &str = "com.trafficview.Daemon1";
const OBJECT_PATH: &str = "/com/trafficview/Daemon1";
const MENU_PATH: &str = "/MenuBar";
const ITEM_PATH: &str = "/StatusNotifierItem";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

const ITEM_PAUSE: i32 = 1;
const ITEM_OVERLAY: i32 = 2;
const ITEM_QUIT: i32 = 3;
const ITEM_INTERFACES: i32 = 10;
const ITEM_CONFIG: i32 = 20;
const ITEM_SETTINGS: i32 = 21;
const ITEM_AUTO: i32 = 99;
const ITEM_IFACE_BASE: i32 = 100;

#[zbus::proxy(
    interface = "com.trafficview.Daemon1",
    default_service = "com.trafficview.Daemon1",
    default_path = "/com/trafficview/Daemon1"
)]
trait Daemon1 {
    fn get_snapshot(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn active_interface(&self) -> zbus::Result<String>;
    /// 暂停状态只在方法调用时变化且不发 PropertiesChanged，
    /// 必须标记为不缓存，否则代理会一直返回首次读到的 false。
    #[zbus(property(emits_changed_signal = "false"))]
    fn paused(&self) -> zbus::Result<bool>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn overlay_visible(&self) -> zbus::Result<bool>;
    #[zbus(property(emits_changed_signal = "false"))]
    fn overlay_show_in_taskbar(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn sample(&self, snapshot: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(signal)]
    fn config_changed(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn alert(&self, level: String, code: String, message: String) -> zbus::Result<()>;
}

type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

#[derive(Default)]
struct Model {
    snapshot: Option<Snapshot>,
    paused: bool,
    overlay_visible: bool,
    show_in_taskbar: bool,
    interfaces: Vec<String>,
    active_interface: String,
    config_path: Option<PathBuf>,
    last_icon: Vec<u8>,
    online: bool,
    /// 最近一条仍 active 的告警 `(code, message)`。
    alert: Option<(String, String)>,
}

impl Model {
    fn title(&self) -> String {
        if !self.show_in_taskbar {
            return self.base_title();
        }
        // "Show in taskbar" 开启时把实时指标塞到 title 里，让 dock 直接显示。
        match (&self.snapshot, self.online, self.paused) {
            (_, false, _) => self.base_title(),
            (_, true, true) => format!("⏸ {} · {}% · {}%", self.base_title(), 0, 0),
            (Some(s), true, false) => format!(
                "↓{} ↑{} · C{:.0} M{:.0}",
                format_rate(s.net.rx_bps),
                format_rate(s.net.tx_bps),
                s.cpu.total_pct,
                s.mem.used_pct
            ),
            (None, true, false) => self.base_title(),
        }
    }

    fn base_title(&self) -> String {
        if !self.online {
            "TrafficView (daemon offline)".into()
        } else if self.paused {
            "TrafficView (paused)".into()
        } else {
            "TrafficView".into()
        }
    }

    fn tool_tip(&self) -> (String, String) {
        let (title, mut body) = match (&self.snapshot, self.online, self.paused) {
            (_, false, _) => ("TrafficView".into(), "daemon offline; retrying…".into()),
            (_, true, true) => ("TrafficView (paused)".into(), String::new()),
            (Some(s), true, false) => {
                let title = format!(
                    "TrafficView · ↓ {}  ↑ {}",
                    format_rate(s.net.rx_bps),
                    format_rate(s.net.tx_bps)
                );
                let mut body = vec![
                    format!("Interface: {}", s.net.iface),
                    format!(
                        "Network:  ↓ {}  ↑ {}   (total ↓ {} ↑ {})",
                        format_rate(s.net.rx_bps),
                        format_rate(s.net.tx_bps),
                        format_bytes(s.net.rx_total),
                        format_bytes(s.net.tx_total),
                    ),
                    format!(
                        "CPU:      {:.0}%{}",
                        s.cpu.total_pct,
                        s.cpu
                            .freq_mhz
                            .map(|f| format!("  @ {} MHz", f))
                            .unwrap_or_default()
                    ),
                    format!(
                        "Memory:   {:.0}%  (used {} / total {})",
                        s.mem.used_pct,
                        format_bytes(s.mem.total - s.mem.available),
                        format_bytes(s.mem.total),
                    ),
                ];
                if s.mem.swap_pct > 0.0 {
                    body.push(format!("Swap:     {:.0}%", s.mem.swap_pct));
                }
                if let Some(d) = &s.disk {
                    body.push(format!(
                        "Disk I/O: ↓ {}  ↑ {}",
                        format_rate(d.read_bps),
                        format_rate(d.write_bps),
                    ));
                }
                if let Some(b) = &s.battery {
                    body.push(format!("Battery:  {:.0}%  {}", b.percent, b.status));
                }
                if !s.temps.is_empty() {
                    let parts: Vec<String> = s
                        .temps
                        .iter()
                        .map(|t| format!("{} {:.0}°C", t.label, t.celsius))
                        .collect();
                    body.push(format!("Temp:     {}", parts.join(" · ")));
                }
                (title, body.join("\n"))
            }
            (None, true, false) => ("TrafficView".into(), "waiting for first sample…".into()),
        };
        match &self.alert {
            Some((_, message)) if self.online => {
                if body.is_empty() {
                    body = format!("⚠ {message}");
                } else {
                    body = format!("{body}\n⚠ {message}");
                }
            }
            _ => {}
        }
        (title, body)
    }

    fn icon(&self) -> trafficview_render::TrayIcon {
        match &self.snapshot {
            Some(snapshot) => tray_icon(snapshot),
            None => tray_icon(&empty_snapshot()),
        }
    }

    /// `Alert(level, code, message)` 的 `info` 级别视为恢复事件。
    fn apply_alert(&mut self, level: &str, code: &str, message: &str) {
        if level.eq_ignore_ascii_case("info") {
            if self
                .alert
                .as_ref()
                .is_some_and(|(active, _)| active == code)
            {
                self.alert = None;
            }
        } else {
            self.alert = Some((code.to_owned(), message.to_owned()));
        }
    }
}

struct StatusNotifierItem {
    model: Arc<RwLock<Model>>,
    daemon: zbus::Connection,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".into()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        "trafficview".into()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.model.read().expect("tray model poisoned").title()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        "trafficview".into()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let icon = self.model.read().expect("tray model poisoned").icon();
        vec![(icon.width, icon.height, icon.argb)]
    }

    #[zbus(property)]
    fn tool_tip(&self) -> ToolTip {
        let model = self.model.read().expect("tray model poisoned");
        let (title, body) = model.tool_tip();
        // ToolTip 四元组 = (icon_name, icon_data, subtitle, body)
        // Deepin Dock 只把 subtitle (字段 [2]) 当 tooltip 主文本渲染，
        // icon_data 传空数组（它自己有 tray icon）。
        ("trafficview".into(), Vec::new(), title, body)
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).expect("static menu path")
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    async fn activate(&self, _x: i32, _y: i32, #[zbus(signal_context)] context: SignalContext<'_>) {
        let visible = {
            let mut model = self.model.write().expect("tray model poisoned");
            model.overlay_visible = !model.overlay_visible;
            model.overlay_visible
        };
        let _ = call_daemon(&self.daemon, "SetOverlayVisible", &(visible,)).await;
        let _ = Self::new_tool_tip(&context).await;
    }

    async fn secondary_activate(&self, _x: i32, _y: i32) {}

    async fn context_menu(&self, _x: i32, _y: i32) {}

    async fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    pub async fn new_icon(context: &SignalContext<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_tool_tip(context: &SignalContext<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_title(context: &SignalContext<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_status(context: &SignalContext<'_>, status: String) -> zbus::Result<()>;
}

struct DbusMenu {
    model: Arc<RwLock<Model>>,
    daemon: zbus::Connection,
}

impl DbusMenu {
    fn item_properties(id: i32, model: &Model) -> HashMap<String, Value<'static>> {
        let mut map: HashMap<String, Value<'static>> = HashMap::new();
        match id {
            ITEM_PAUSE => {
                map.insert(
                    "label".into(),
                    Value::from(if model.paused { "Resume" } else { "Pause" }),
                );
            }
            ITEM_OVERLAY => {
                map.insert(
                    "label".into(),
                    Value::from(if model.overlay_visible {
                        "Hide overlay"
                    } else {
                        "Show overlay"
                    }),
                );
            }
            ITEM_QUIT => {
                map.insert("label".into(), Value::from("Quit TrafficView"));
            }
            ITEM_INTERFACES => {
                map.insert("label".into(), Value::from("Network interface"));
                map.insert("children-display".into(), Value::from("submenu"));
            }
            ITEM_SETTINGS => {
                map.insert("label".into(), Value::from("Settings…"));
            }
            ITEM_CONFIG => {
                map.insert("label".into(), Value::from("Open config"));
            }
            ITEM_AUTO => {
                map.insert("label".into(), Value::from("Automatic"));
                map.insert("type".into(), Value::from("radio"));
                map.insert(
                    "toggle-state".into(),
                    Value::from(if model.active_interface.is_empty() {
                        1i32
                    } else {
                        0i32
                    }),
                );
            }
            id if id >= ITEM_IFACE_BASE => {
                let name = model
                    .interfaces
                    .get((id - ITEM_IFACE_BASE) as usize)
                    .cloned()
                    .unwrap_or_default();
                map.insert("label".into(), Value::from(name.clone()));
                map.insert("type".into(), Value::from("radio"));
                map.insert(
                    "toggle-state".into(),
                    Value::from(if model.active_interface == name {
                        1i32
                    } else {
                        0i32
                    }),
                );
            }
            _ => {}
        }
        map.insert("enabled".into(), Value::from(true));
        map.insert("visible".into(), Value::from(true));
        map
    }

    fn item(id: i32, model: &Model, children: Vec<OwnedValue>) -> OwnedValue {
        let map = Self::item_properties(id, model);
        OwnedValue::try_from(Value::from((id, map, children))).expect("menu item is owned")
    }

    async fn items(&self) -> Vec<OwnedValue> {
        let model = self.model.read().expect("tray model poisoned");
        let mut interface_items = vec![Self::item(ITEM_AUTO, &model, Vec::new())];
        for index in 0..model.interfaces.len() {
            interface_items.push(Self::item(
                ITEM_IFACE_BASE + index as i32,
                &model,
                Vec::new(),
            ));
        }
        vec![
            Self::item(ITEM_PAUSE, &model, Vec::new()),
            Self::item(ITEM_OVERLAY, &model, Vec::new()),
            Self::item(ITEM_INTERFACES, &model, interface_items),
            Self::item(ITEM_SETTINGS, &model, Vec::new()),
            Self::item(ITEM_CONFIG, &model, Vec::new()),
            Self::item(ITEM_QUIT, &model, Vec::new()),
        ]
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    async fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>))> {
        let mut properties = HashMap::new();
        properties.insert(
            "children-display".to_string(),
            OwnedValue::try_from(Value::from("submenu"))
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
        );
        Ok(((1u32), (0, properties, self.items().await)))
    }

    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let model = self.model.read().expect("tray model poisoned");
        let ids = if ids.is_empty() {
            let mut ids = vec![
                0,
                ITEM_PAUSE,
                ITEM_OVERLAY,
                ITEM_INTERFACES,
                ITEM_SETTINGS,
                ITEM_CONFIG,
                ITEM_QUIT,
                ITEM_AUTO,
            ];
            ids.extend((0..model.interfaces.len()).map(|index| ITEM_IFACE_BASE + index as i32));
            ids
        } else {
            ids
        };
        ids.into_iter()
            .map(|id| {
                let props = Self::item_properties(id, &model)
                    .into_iter()
                    .filter_map(|(key, value)| {
                        OwnedValue::try_from(value).ok().map(|value| (key, value))
                    })
                    .collect();
                (id, props)
            })
            .collect()
    }

    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let model = self.model.read().expect("tray model poisoned");
        Self::item_properties(id, &model)
            .remove(&name)
            .and_then(|value| OwnedValue::try_from(value).ok())
            .ok_or_else(|| zbus::fdo::Error::UnknownProperty(name))
    }

    async fn event(
        &self,
        id: i32,
        event_id: String,
        _data: OwnedValue,
        _timestamp: u32,
        #[zbus(signal_context)] context: SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        if self.apply_event(id, &event_id).await? {
            let _ = Self::layout_updated(&context, 1, 0).await;
        }
        Ok(())
    }

    /// Cinnamon/xapp-sn-watcher 等宿主用 `EventGroup` 批量投递点击；
    /// 只实现 `Event` 会导致菜单项点击被宿主静默丢弃。
    async fn event_group(
        &self,
        events: Vec<(i32, String, OwnedValue, u32)>,
        #[zbus(signal_context)] context: SignalContext<'_>,
    ) -> zbus::fdo::Result<()> {
        let mut changed = false;
        for (id, event_id, _data, _timestamp) in events {
            changed |= self.apply_event(id, &event_id).await?;
        }
        if changed {
            let _ = Self::layout_updated(&context, 1, 0).await;
        }
        Ok(())
    }

    async fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<OwnedObjectPath>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    async fn about_to_show(&self, _id: i32) -> bool {
        true
    }

    /// 返回是否改变了菜单状态（需要广播 LayoutUpdated）。
    async fn apply_event(&self, id: i32, event_id: &str) -> zbus::fdo::Result<bool> {
        if event_id != "clicked" {
            return Ok(false);
        }
        let action = match id {
            ITEM_PAUSE => Some((
                "Pause",
                {
                    let mut model = self.model.write().expect("tray model poisoned");
                    model.paused = !model.paused;
                    model.paused
                }
                .to_string(),
            )),
            ITEM_OVERLAY => Some((
                "SetOverlayVisible",
                {
                    let mut model = self.model.write().expect("tray model poisoned");
                    model.overlay_visible = !model.overlay_visible;
                    model.overlay_visible
                }
                .to_string(),
            )),
            ITEM_AUTO => Some(("SetActiveInterface", String::new())),
            id if id >= ITEM_IFACE_BASE => {
                let name = {
                    let model = self.model.read().expect("tray model poisoned");
                    model
                        .interfaces
                        .get((id - ITEM_IFACE_BASE) as usize)
                        .cloned()
                };
                name.map(|name| ("SetActiveInterface", name))
            }
            ITEM_SETTINGS => {
                self.open_settings();
                None
            }
            ITEM_CONFIG => {
                self.open_config();
                None
            }
            ITEM_QUIT => {
                let _ = call_daemon(&self.daemon, "Quit", &()).await;
                std::process::exit(0);
            }
            _ => None,
        };
        let Some((method, value)) = action else {
            return Ok(false);
        };
        let result = match method {
            "Pause" => call_daemon(&self.daemon, "Pause", &(value == "true",)).await,
            "SetOverlayVisible" => {
                call_daemon(&self.daemon, "SetOverlayVisible", &(value == "true",)).await
            }
            _ => call_daemon(&self.daemon, "SetActiveInterface", &(value.as_str(),)).await,
        };
        result.map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;

        Ok(true)
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".into()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".into()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(signal)]
    pub async fn layout_updated(
        context: &SignalContext<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn items_properties_updated(
        context: &SignalContext<'_>,
        updated: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;
}

impl DbusMenu {
    /// 拉起独立设置窗口：优先托盘同目录，其次 PATH；找不到时退回打开配置文件，
    /// 避免出现“点了没反应”。
    fn open_settings(&self) {
        let path = settings_binary();
        match std::process::Command::new(&path).spawn() {
            Ok(_) => {}
            Err(error) => {
                eprintln!("tray: cannot launch {}: {error}", path.display());
                self.open_config();
            }
        }
    }

    fn open_config(&self) {
        let Some(path) = self
            .model
            .read()
            .expect("tray model poisoned")
            .config_path
            .clone()
        else {
            return;
        };
        if let Err(error) = std::process::Command::new("xdg-open").arg(&path).spawn() {
            eprintln!("tray: cannot open {}: {error}", path.display());
        }
    }
}

async fn call_daemon<B>(connection: &zbus::Connection, method: &str, body: &B) -> zbus::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(BUS_NAME), method, body)
        .await?;
    Ok(())
}

/// 设置窗口的位置：与托盘同目录（打包安装后的 `/usr/bin`），否则交给 PATH。
fn settings_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(candidate) = exe.parent().map(|dir| dir.join("trafficview-settings")) {
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("trafficview-settings")
}

/// 启动托盘；阻塞直到 Quit、Ctrl-C 或 bus 断开。
pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the tray runtime")?;
    runtime.block_on(run_async())
}

async fn run_async() -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("cannot connect to the session bus")?;
    // 固定名做单实例：从启动菜单和会话自启动重复拉起时，第二个实例直接退出。
    match connection.request_name("com.trafficview.Tray").await {
        Ok(()) => {}
        Err(zbus::Error::NameTaken) => {
            eprintln!("tray: another instance is already running");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    let store = ConfigStore::xdg().ok();
    let overlay_visible = store
        .as_ref()
        .and_then(|store| store.load().ok())
        .map(|config| config.overlay.visible)
        .unwrap_or(true);
    let show_in_taskbar = store
        .as_ref()
        .and_then(|store| store.load().ok())
        .map(|config| config.overlay.show_in_taskbar)
        .unwrap_or(false);
    let model = Arc::new(RwLock::new(Model {
        overlay_visible,
        show_in_taskbar,
        config_path: store.as_ref().map(|store| store.path().to_path_buf()),
        ..Model::default()
    }));
    connection
        .object_server()
        .at(
            ITEM_PATH,
            StatusNotifierItem {
                model: model.clone(),
                daemon: connection.clone(),
            },
        )
        .await?;
    connection
        .object_server()
        .at(
            MENU_PATH,
            DbusMenu {
                model: model.clone(),
                daemon: connection.clone(),
            },
        )
        .await?;
    let service = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    connection.request_name(service.as_str()).await?;
    register_with_watcher(&connection, &service).await;
    eprintln!("tray: registered as {service}");

    let proxy = Daemon1Proxy::new(&connection)
        .await
        .context("cannot reach trafficviewd; start the daemon first")?;
    let mut samples = proxy.receive_sample().await?;
    let mut config_changes = proxy.receive_config_changed().await?;
    let mut alerts = proxy.receive_alert().await?;
    let item_context = SignalContext::new(&connection, ITEM_PATH)?;
    let menu_context = SignalContext::new(&connection, MENU_PATH)?;
    refresh_interfaces(&proxy, &model).await;
    let mut seconds = 0u32;
    loop {
        tokio::select! {
            Some(signal) = samples.next() => {
                let args = signal.args()?;
                let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&args.snapshot)?;
                let icon = tray_icon(&snapshot).argb;
                let changed = {
                    let mut model = model.write().expect("tray model poisoned");
                    let changed = model.last_icon != icon || !model.online;
                    model.snapshot = Some(snapshot);
                    model.last_icon = icon;
                    model.online = true;
                    changed
                };
                if changed {
                    let _ = StatusNotifierItem::new_icon(&item_context).await;
                }
                // title 每秒都在变（速率、CPU、MEM），emit 让 dock 更新。
                let _ = StatusNotifierItem::new_title(&item_context).await;
                let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
            }
            Some(_) = config_changes.next() => {
                refresh_interfaces(&proxy, &model).await;
                // 从 daemon 拉取最新状态，避免 tray model 与配置漂移。
                if let Ok(paused) = proxy.paused().await {
                    model.write().expect("tray model poisoned").paused = paused;
                }
                if let Ok(visible) = proxy.overlay_visible().await {
                    model.write().expect("tray model poisoned").overlay_visible = visible;
                }
                if let Ok(show) = proxy.overlay_show_in_taskbar().await {
                    model.write().expect("tray model poisoned").show_in_taskbar = show;
                }
                let _ = StatusNotifierItem::new_title(&item_context).await;
                let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
                let _ = DbusMenu::layout_updated(&menu_context, 1, 0).await;
            }
            Some(signal) = alerts.next() => {
                let args = signal.args()?;
                {
                    let mut model = model.write().expect("tray model poisoned");
                    model.apply_alert(&args.level, &args.code, &args.message);
                }
                let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                seconds += 1;
                if !seconds.is_multiple_of(5) {
                    continue;
                }
                // 5 秒内发现 daemon 消失并切换为断开状态。
                match proxy.get_snapshot().await {
                    Ok(_) => {
                        let was_offline = {
                            let mut model = model.write().expect("tray model poisoned");
                            let was_offline = !model.online;
                            model.online = true;
                            was_offline
                        };
                        if was_offline {
                            refresh_interfaces(&proxy, &model).await;
                            let _ = StatusNotifierItem::new_icon(&item_context).await;
                            let _ = StatusNotifierItem::new_title(&item_context).await;
                            let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
                        }
                    }
                    Err(_) => {
                        // 暂停时 GetSnapshot 也会报错；此时 daemon 仍然在线，不能显示离线。
                        if proxy.paused().await.unwrap_or(false) {
                            let was_offline = {
                                let mut model = model.write().expect("tray model poisoned");
                                let was_offline = !model.online;
                                model.online = true;
                                model.paused = true;
                                was_offline
                            };
                            if was_offline {
                                refresh_interfaces(&proxy, &model).await;
                                let _ = StatusNotifierItem::new_title(&item_context).await;
                            }
                            continue;
                        }
                        {
                            let mut model = model.write().expect("tray model poisoned");
                            model.online = false;
                            model.snapshot = None;
                            model.paused = false;
                        }
                        let _ = StatusNotifierItem::new_icon(&item_context).await;
                        let _ = StatusNotifierItem::new_title(&item_context).await;
                        let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn refresh_interfaces(proxy: &Daemon1Proxy<'_>, model: &Arc<RwLock<Model>>) {
    let interfaces = proxy.interfaces().await.unwrap_or_default();
    let active = proxy.active_interface().await.unwrap_or_default();
    let mut model = model.write().expect("tray model poisoned");
    model.interfaces = interfaces;
    model.active_interface = active;
}

async fn register_with_watcher(connection: &zbus::Connection, service: &str) {
    let reply = connection
        .call_method(
            Some(WATCHER),
            WATCHER_PATH,
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(service,),
        )
        .await;
    if reply.is_ok() {
        return;
    }
    if let Err(error) = connection
        .call_method(
            Some(WATCHER),
            WATCHER_PATH,
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(ITEM_PATH,),
        )
        .await
    {
        eprintln!("tray: StatusNotifierWatcher unavailable: {error:#}");
    }
}

fn empty_snapshot() -> Snapshot {
    Snapshot::new(
        0,
        trafficview_core::NetSample {
            iface: String::new(),
            rx_bps: 0,
            tx_bps: 0,
            rx_total: 0,
            tx_total: 0,
        },
        trafficview_core::CpuSample {
            total_pct: 0.0,
            per_core: Vec::new(),
            freq_mhz: None,
            load: None,
        },
        trafficview_core::MemSample {
            used_pct: 0.0,
            total: 0,
            available: 0,
            swap_pct: 0.0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_are_unique_and_interface_entries_follow_auto() {
        let model = Model {
            interfaces: vec!["eth0".into(), "wlan0".into()],
            active_interface: "wlan0".into(),
            overlay_visible: true,
            ..Model::default()
        };
        let auto = DbusMenu::item_properties(ITEM_AUTO, &model);
        let wlan = DbusMenu::item_properties(ITEM_IFACE_BASE + 1, &model);
        assert_eq!(auto["toggle-state"].try_clone().unwrap(), Value::from(0i32));
        assert_eq!(wlan["toggle-state"].try_clone().unwrap(), Value::from(1i32));
        assert_eq!(wlan["label"].try_clone().unwrap(), Value::from("wlan0"));
    }

    #[test]
    fn offline_and_paused_states_are_labelled() {
        let mut model = Model::default();
        assert!(model.title().contains("offline"));
        model.online = true;
        model.paused = true;
        assert!(model.title().contains("paused"));
        model.paused = false;
        assert_eq!(model.title(), "TrafficView");
    }

    #[test]
    fn settings_entry_has_a_label() {
        let model = Model::default();
        let settings = DbusMenu::item_properties(ITEM_SETTINGS, &model);
        assert_eq!(
            settings["label"].try_clone().unwrap(),
            Value::from("Settings…")
        );
    }
}
