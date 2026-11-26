//! StatusNotifierItem + com.canonical.dbusmenu，纯 Rust（zbus）实现，仅消费 D-Bus。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use trafficview_core::i18n::{self, Language};
use trafficview_core::{config::ConfigStore, format::format_bytes, format::format_rate, Snapshot};
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
    fn get_config(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

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

struct Model {
    snapshot: Option<Snapshot>,
    paused: bool,
    overlay_visible: bool,
    show_in_taskbar: bool,
    interfaces: Vec<String>,
    active_interface: String,
    lang: Language,
    config_path: Option<PathBuf>,
    online: bool,
    /// 最近一条仍 active 的告警 `(code, message)`。
    alert: Option<(String, String)>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            snapshot: None,
            paused: false,
            overlay_visible: false,
            show_in_taskbar: false,
            interfaces: Vec::new(),
            active_interface: String::new(),
            lang: Language::En,
            config_path: None,
            online: false,
            alert: None,
        }
    }
}

impl Model {
    fn title(&self) -> String {
        if !self.show_in_taskbar {
            return self.base_title();
        }
        // "Show in taskbar" 开启时把实时指标塞到 title 里，让 dock 直接显示。
        match (&self.snapshot, self.online, self.paused) {
            (_, false, _) => self.base_title(),
            (_, true, true) => i18n::trf(
                self.lang,
                "tray.title.paused_metrics",
                &[("title", &self.base_title()), ("cpu", "0"), ("memory", "0")],
            ),
            (Some(s), true, false) => {
                let download = format_rate(s.net.rx_bps);
                let upload = format_rate(s.net.tx_bps);
                let cpu = format!("{:.0}", s.cpu.total_pct);
                let memory = format!("{:.0}", s.mem.used_pct);
                i18n::trf(
                    self.lang,
                    "tray.title.metrics",
                    &[
                        ("download", &download),
                        ("upload", &upload),
                        ("cpu", &cpu),
                        ("memory", &memory),
                    ],
                )
            }
            (None, true, false) => self.base_title(),
        }
    }

    fn base_title(&self) -> String {
        if !self.online {
            i18n::tr(self.lang, "tray.title.offline").into()
        } else if self.paused {
            i18n::tr(self.lang, "tray.title.paused").into()
        } else {
            i18n::tr(self.lang, "tray.app_name").into()
        }
    }

    fn tool_tip(&self) -> (String, String) {
        let (title, mut body) = match (&self.snapshot, self.online, self.paused) {
            (_, false, _) => (
                i18n::tr(self.lang, "tray.app_name").into(),
                i18n::tr(self.lang, "tray.tooltip.offline").into(),
            ),
            (_, true, true) => (
                i18n::tr(self.lang, "tray.title.paused").into(),
                String::new(),
            ),
            (Some(s), true, false) => {
                let download = format_rate(s.net.rx_bps);
                let upload = format_rate(s.net.tx_bps);
                let title = i18n::trf(
                    self.lang,
                    "tray.tooltip.title",
                    &[("download", &download), ("upload", &upload)],
                );
                let download_total = format_bytes(s.net.rx_total);
                let upload_total = format_bytes(s.net.tx_total);
                let frequency = s
                    .cpu
                    .freq_mhz
                    .map(|value| {
                        let value = value.to_string();
                        i18n::trf(
                            self.lang,
                            "tray.tooltip.cpu_frequency",
                            &[("value", &value)],
                        )
                    })
                    .unwrap_or_default();
                let cpu = format!("{:.0}", s.cpu.total_pct);
                let memory = format!("{:.0}", s.mem.used_pct);
                let memory_used = format_bytes(s.mem.total - s.mem.available);
                let memory_total = format_bytes(s.mem.total);
                let mut body = vec![
                    i18n::trf(
                        self.lang,
                        "tray.tooltip.interface",
                        &[("interface", &s.net.iface)],
                    ),
                    i18n::trf(
                        self.lang,
                        "tray.tooltip.network",
                        &[
                            ("download", &download),
                            ("upload", &upload),
                            ("download_total", &download_total),
                            ("upload_total", &upload_total),
                        ],
                    ),
                    i18n::trf(
                        self.lang,
                        "tray.tooltip.cpu",
                        &[("value", &cpu), ("frequency", &frequency)],
                    ),
                    i18n::trf(
                        self.lang,
                        "tray.tooltip.memory",
                        &[
                            ("value", &memory),
                            ("used", &memory_used),
                            ("total", &memory_total),
                        ],
                    ),
                ];
                if s.mem.swap_pct > 0.0 {
                    let value = format!("{:.0}", s.mem.swap_pct);
                    body.push(i18n::trf(
                        self.lang,
                        "tray.tooltip.swap",
                        &[("value", &value)],
                    ));
                }
                if let Some(d) = &s.disk {
                    let read = format_rate(d.read_bps);
                    let write = format_rate(d.write_bps);
                    body.push(i18n::trf(
                        self.lang,
                        "tray.tooltip.disk",
                        &[("read", &read), ("write", &write)],
                    ));
                }
                if let Some(b) = &s.battery {
                    let value = format!("{:.0}", b.percent);
                    body.push(i18n::trf(
                        self.lang,
                        "tray.tooltip.battery",
                        &[("value", &value), ("status", &b.status)],
                    ));
                }
                if !s.temps.is_empty() {
                    let parts: Vec<String> = s
                        .temps
                        .iter()
                        .map(|t| {
                            let value = format!("{:.0}", t.celsius);
                            i18n::trf(
                                self.lang,
                                "tray.tooltip.temp_sensor",
                                &[("label", &t.label), ("value", &value)],
                            )
                        })
                        .collect();
                    let value = parts.join(" · ");
                    body.push(i18n::trf(
                        self.lang,
                        "tray.tooltip.temp",
                        &[("value", &value)],
                    ));
                }
                (title, body.join("\n"))
            }
            (None, true, false) => (
                i18n::tr(self.lang, "tray.app_name").into(),
                i18n::tr(self.lang, "tray.tooltip.waiting").into(),
            ),
        };
        match &self.alert {
            Some((_, message)) if self.online => {
                let alert = i18n::trf(
                    self.lang,
                    "tray.tooltip.alert",
                    &[("message", message.as_str())],
                );
                if body.is_empty() {
                    body = alert;
                } else {
                    body = format!("{body}\n{alert}");
                }
            }
            _ => {}
        }
        (title, body)
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
        let (width, height, argb) = static_icon();
        vec![(*width, *height, argb.clone())]
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
            0 => {
                map.insert("children-display".into(), Value::from("submenu"));
            }
            ITEM_PAUSE => {
                let key = if model.paused {
                    "tray.menu.resume"
                } else {
                    "tray.menu.pause"
                };
                map.insert("label".into(), Value::from(i18n::tr(model.lang, key)));
            }
            ITEM_OVERLAY => {
                let key = if model.overlay_visible {
                    "tray.menu.hide_overlay"
                } else {
                    "tray.menu.show_overlay"
                };
                map.insert("label".into(), Value::from(i18n::tr(model.lang, key)));
            }
            ITEM_QUIT => {
                map.insert(
                    "label".into(),
                    Value::from(i18n::tr(model.lang, "tray.menu.quit")),
                );
            }
            ITEM_INTERFACES => {
                map.insert(
                    "label".into(),
                    Value::from(i18n::tr(model.lang, "tray.menu.network_interface")),
                );
                map.insert("children-display".into(), Value::from("submenu"));
            }
            ITEM_SETTINGS => {
                map.insert(
                    "label".into(),
                    Value::from(i18n::tr(model.lang, "tray.menu.settings")),
                );
            }
            ITEM_CONFIG => {
                map.insert(
                    "label".into(),
                    Value::from(i18n::tr(model.lang, "tray.menu.open_config")),
                );
            }
            ITEM_AUTO => {
                map.insert(
                    "label".into(),
                    Value::from(i18n::tr(model.lang, "tray.menu.automatic")),
                );
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

    fn child_ids(id: i32, model: &Model) -> Vec<i32> {
        match id {
            0 => vec![
                ITEM_PAUSE,
                ITEM_OVERLAY,
                ITEM_INTERFACES,
                ITEM_SETTINGS,
                ITEM_CONFIG,
                ITEM_QUIT,
            ],
            ITEM_INTERFACES => {
                let mut ids = vec![ITEM_AUTO];
                ids.extend((0..model.interfaces.len()).map(|index| ITEM_IFACE_BASE + index as i32));
                ids
            }
            _ => Vec::new(),
        }
    }

    /// DBusMenu 的 `recursion-depth`：0 不带子项，-1 递归到底，正数只展开对应层数。
    fn children(id: i32, depth: i32, model: &Model) -> Vec<OwnedValue> {
        if depth == 0 {
            return Vec::new();
        }
        Self::child_ids(id, model)
            .into_iter()
            .map(|child| Self::item(child, model, Self::children(child, depth - 1, model)))
            .collect()
    }

    fn layout(
        id: i32,
        depth: i32,
        model: &Model,
    ) -> (i32, HashMap<String, Value<'static>>, Vec<OwnedValue>) {
        (
            id,
            Self::item_properties(id, model),
            Self::children(id, depth, model),
        )
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>))> {
        let model = self.model.read().expect("tray model poisoned");
        let (id, properties, children) = Self::layout(parent_id, recursion_depth, &model);
        let properties = properties
            .into_iter()
            .map(|(key, value)| {
                OwnedValue::try_from(value)
                    .map(|value| (key, value))
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
            })
            .collect::<zbus::fdo::Result<HashMap<_, _>>>()?;
        Ok((1u32, (id, properties, children)))
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
    refresh_config(&proxy, &model).await;
    let mut seconds = 0u32;
    loop {
        tokio::select! {
            Some(signal) = samples.next() => {
                let args = signal.args()?;
                let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&args.snapshot)?;
                {
                    let mut model = model.write().expect("tray model poisoned");
                    model.snapshot = Some(snapshot);
                    model.online = true;
                }
                // title 每秒都在变（速率、CPU、MEM），emit 让 dock 更新。
                let _ = StatusNotifierItem::new_title(&item_context).await;
                let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
            }
            Some(_) = config_changes.next() => {
                refresh_config(&proxy, &model).await;
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
                            refresh_config(&proxy, &model).await;
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
                                refresh_config(&proxy, &model).await;
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
                        let _ = StatusNotifierItem::new_title(&item_context).await;
                        let _ = StatusNotifierItem::new_tool_tip(&item_context).await;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn refresh_config(proxy: &Daemon1Proxy<'_>, model: &Arc<RwLock<Model>>) {
    let interfaces = proxy.interfaces().await.unwrap_or_default();
    let active = proxy.active_interface().await.unwrap_or_default();
    let lang = proxy
        .get_config()
        .await
        .ok()
        .and_then(|config| {
            config
                .get("language")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| String::try_from(value).ok())
        })
        .map(|language| i18n::resolve(&language))
        .unwrap_or(Language::En);
    let mut model = model.write().expect("tray model poisoned");
    model.interfaces = interfaces;
    model.active_interface = active;
    model.lang = lang;
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

/// 包内的 64px 应用图标，托盘固定使用它，不再随速率重绘。
const STATIC_ICON_PNG: &[u8] =
    include_bytes!("../../../packaging/icons/hicolor/64x64/apps/trafficview.png");

/// 解码一次并转成 StatusNotifierItem 要求的 ARGB32（网络字节序）。
fn static_icon() -> &'static (i32, i32, Vec<u8>) {
    static ICON: OnceLock<(i32, i32, Vec<u8>)> = OnceLock::new();
    ICON.get_or_init(|| {
        let mut decoder = png::Decoder::new(STATIC_ICON_PNG);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .expect("bundled tray icon is a valid PNG");
        let mut buffer = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buffer)
            .expect("bundled tray icon has a frame");
        let argb = match info.color_type {
            png::ColorType::Rgba => buffer
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|pixel| [pixel[3], pixel[0], pixel[1], pixel[2]])
                .collect(),
            png::ColorType::Rgb => buffer
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|pixel| [255, pixel[0], pixel[1], pixel[2]])
                .collect(),
            other => panic!("unsupported tray icon color type: {other:?}"),
        };
        (info.width as i32, info.height as i32, argb)
    })
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
    fn tray_labels_follow_language() {
        use trafficview_core::i18n::{tr, Language};

        assert_eq!(tr(Language::Zh, "tray.menu.pause"), "暂停");
        assert_eq!(tr(Language::En, "tray.menu.pause"), "Pause");

        let model = Model {
            lang: Language::Zh,
            ..Model::default()
        };
        let pause = DbusMenu::item_properties(ITEM_PAUSE, &model);
        assert_eq!(pause["label"].try_clone().unwrap(), Value::from("暂停"));
    }

    #[test]
    fn static_tray_icon_is_a_decoded_app_icon() {
        let (width, height, argb) = static_icon();
        assert_eq!((*width, *height), (64, 64));
        assert_eq!(argb.len(), 64 * 64 * 4);
        assert!(argb.as_chunks::<4>().0.iter().any(|pixel| pixel[0] != 0));
    }

    #[test]
    fn layout_returns_interface_submenu_for_its_own_parent_id() {
        let model = Model {
            interfaces: vec!["eth0".into(), "wlan0".into()],
            ..Model::default()
        };

        let (root_id, root_props, root_children) = DbusMenu::layout(0, 1, &model);
        assert_eq!(root_id, 0);
        assert_eq!(
            root_props["children-display"].try_clone().unwrap(),
            Value::from("submenu")
        );
        assert_eq!(root_children.len(), 6);

        let (id, props, children) = DbusMenu::layout(ITEM_INTERFACES, 1, &model);
        assert_eq!(id, ITEM_INTERFACES);
        assert_eq!(
            props["label"].try_clone().unwrap(),
            Value::from("Network interface")
        );
        assert_eq!(
            DbusMenu::child_ids(ITEM_INTERFACES, &model),
            vec![ITEM_AUTO, ITEM_IFACE_BASE, ITEM_IFACE_BASE + 1]
        );
        assert_eq!(children.len(), 3);
        assert!(DbusMenu::layout(ITEM_QUIT, -1, &model).2.is_empty());
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
