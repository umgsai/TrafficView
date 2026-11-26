//! GTK4/libadwaita 设置窗口。
//!
//! 七个页面：General / Network / Display / Alerts / History / Plugins & Advanced / About。
//! 无 daemon 时禁用全部编辑控件并在顶部横幅提示重连；所有保存走 D-Bus `UpdateConfig`，
//! 绝不直接写 daemon 配置文件，也不提供自动提权/pkexec 按钮。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use zbus::zvariant::OwnedValue;

use crate::autostart;
use crate::config_model::{UiAlertRule, UiConfig, UiItem};
use crate::dbus::{ClientEvent, DaemonClient};

const SAMPLE_MIN: f64 = 500.0;
const SAMPLE_MAX: f64 = 30_000.0;
const RETENTION_MIN: f64 = 1.0;
const RETENTION_MAX: f64 = 3650.0;
const OPACITY_MIN: f64 = 0.1;
const OPACITY_MAX: f64 = 1.0;

fn split_csv(text: &str) -> Vec<String> {
    text.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn join_csv(items: &[String]) -> String {
    items.join(", ")
}

fn adj(value: f64, min: f64, max: f64, step: f64) -> gtk::Adjustment {
    gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0)
}

fn combo(options: &[&str]) -> (adw::ComboRow, gtk::StringList) {
    let model = gtk::StringList::new(options);
    let row = adw::ComboRow::new();
    row.set_model(Some(&model));
    (row, model)
}

fn combo_selected(row: &adw::ComboRow) -> Option<String> {
    let idx = row.selected();
    let model = row.model()?.downcast::<gtk::StringList>().ok()?;
    model.string(idx).map(|s| s.to_string())
}

fn combo_set(row: &adw::ComboRow, value: &str) {
    if let Some(model) = row
        .model()
        .and_then(|m| m.downcast::<gtk::StringList>().ok())
    {
        for i in 0..model.n_items() {
            if model.string(i).as_deref() == Some(value) {
                row.set_selected(i);
                return;
            }
        }
    }
}

/// “自动选择”在 D-Bus 契约里是空字符串，只在界面上显示成文案。
const AUTO_INTERFACE_LABEL: &str = "（自动选择默认网卡）";

fn interface_value(selected: &str) -> String {
    if selected == AUTO_INTERFACE_LABEL {
        String::new()
    } else {
        selected.to_string()
    }
}

fn set_active_interface_row(row: &adw::ComboRow, value: &str) {
    if value.is_empty() {
        row.set_selected(0);
    } else {
        combo_set(row, value);
    }
}

/// 主应用状态：D-Bus 客户端、当前配置快照与全部控件句柄。
struct AppState {
    client: DaemonClient,
    config: RefCell<UiConfig>,
    dirty: Cell<bool>,
    loading: Cell<bool>,
    connected: Cell<bool>,

    banner: adw::Banner,
    toast_overlay: adw::ToastOverlay,

    // General
    sample_interval: adw::SpinRow,
    autostart: adw::SwitchRow,

    // Network
    active_interface: adw::ComboRow,
    interface_model: gtk::StringList,
    blacklist: adw::EntryRow,
    whitelist: adw::EntryRow,
    smoothing: adw::ComboRow,
    memory_include_cache: adw::SwitchRow,

    // Display
    theme: adw::ComboRow,
    overlay_visible: adw::SwitchRow,
    overlay_opacity: adw::SpinRow,
    overlay_units: adw::ComboRow,
    overlay_always_on_top: adw::SwitchRow,
    overlay_click_through: adw::SwitchRow,
    overlay_pos_x: adw::SpinRow,
    overlay_pos_y: adw::SpinRow,
    overlay_skin: adw::EntryRow,
    overlay_items_box: gtk::ListBox,

    // Alerts
    alerts_box: gtk::ListBox,

    // History
    retention_days: adw::SpinRow,
    timezone: adw::EntryRow,

    // Plugins / Advanced
    plugins_enabled: adw::SwitchRow,
    process_backend: adw::ComboRow,
    collector_box: gtk::ListBox,

    editable: Vec<gtk::Widget>,
}

impl AppState {
    fn mark_dirty(&self) {
        if self.loading.get() {
            return;
        }
        self.dirty.set(true);
    }

    fn handle_event(&self, event: ClientEvent) {
        match event {
            ClientEvent::Connected => {
                self.connected.set(true);
                self.set_connected_ui(true, "");
            }
            ClientEvent::Disconnected(reason) => {
                self.connected.set(false);
                self.set_connected_ui(false, &reason);
            }
            ClientEvent::Config(cfg) => {
                // 有未保存修改时不覆盖界面，避免丢失用户输入。
                if !self.dirty.get() {
                    self.apply_config(&cfg);
                }
                self.config.replace(cfg);
            }
            ClientEvent::Interfaces(ifaces) => {
                self.rebuild_interface_model(&ifaces);
            }
            ClientEvent::CollectorStatus(map) => {
                self.rebuild_collector_status(&map);
            }
            ClientEvent::UpdateResult(result) => match result {
                Ok(()) => {
                    self.dirty.set(false);
                    self.show_toast("已保存");
                }
                Err(reason) => {
                    self.show_toast(&format!("保存失败：{reason}"));
                }
            },
        }
    }

    fn set_connected_ui(&self, connected: bool, reason: &str) {
        for widget in &self.editable {
            widget.set_sensitive(connected);
        }
        if connected {
            self.banner.set_revealed(false);
        } else {
            self.banner
                .set_title(&format!("无法连接到 TrafficView 守护进程：{reason}"));
            self.banner.set_revealed(true);
        }
    }

    fn show_toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        self.toast_overlay.add_toast(toast);
    }

    /// 把配置快照填入控件；期间禁止标记 dirty。
    fn apply_config(&self, cfg: &UiConfig) {
        self.loading.set(true);
        self.sample_interval
            .set_value(f64::from(cfg.sample_interval_ms));
        self.autostart.set_active(autostart::is_enabled());

        set_active_interface_row(&self.active_interface, &cfg.active_interface);
        self.blacklist.set_text(&join_csv(&cfg.interface_blacklist));
        self.whitelist.set_text(&join_csv(&cfg.interface_whitelist));
        combo_set(&self.smoothing, &cfg.network_smoothing);
        self.memory_include_cache
            .set_active(cfg.memory_include_cache);

        combo_set(&self.theme, &cfg.theme);
        self.overlay_visible.set_active(cfg.overlay_visible);
        self.overlay_opacity.set_value(cfg.overlay_opacity);
        combo_set(&self.overlay_units, &cfg.overlay_units);
        self.overlay_always_on_top
            .set_active(cfg.overlay_always_on_top);
        self.overlay_click_through
            .set_active(cfg.overlay_click_through);
        self.overlay_pos_x
            .set_value(f64::from(cfg.overlay_position[0]));
        self.overlay_pos_y
            .set_value(f64::from(cfg.overlay_position[1]));
        self.overlay_skin.set_text(&cfg.overlay_skin);
        self.rebuild_items(&cfg.overlay_items);

        self.rebuild_alerts(&cfg.alerts_rules);

        self.retention_days
            .set_value(f64::from(cfg.history_retention_days));
        self.timezone.set_text(&cfg.history_timezone);

        self.plugins_enabled.set_active(cfg.plugins_enabled);
        combo_set(&self.process_backend, &cfg.process_traffic_backend);
        self.loading.set(false);
    }

    /// 从控件读取当前值，组装成待提交的配置快照。
    fn collect_config(&self) -> UiConfig {
        let mut cfg = self.config.borrow().clone();
        cfg.sample_interval_ms = self.sample_interval.value() as u32;
        cfg.active_interface =
            interface_value(&combo_selected(&self.active_interface).unwrap_or_default());
        cfg.interface_blacklist = split_csv(&self.blacklist.text());
        cfg.interface_whitelist = split_csv(&self.whitelist.text());
        cfg.network_smoothing = combo_selected(&self.smoothing).unwrap_or_else(|| "none".into());
        cfg.memory_include_cache = self.memory_include_cache.is_active();

        cfg.theme = combo_selected(&self.theme).unwrap_or_else(|| "dark".into());
        cfg.overlay_visible = self.overlay_visible.is_active();
        cfg.overlay_opacity = self.overlay_opacity.value();
        cfg.overlay_units = combo_selected(&self.overlay_units).unwrap_or_else(|| "bytes".into());
        cfg.overlay_always_on_top = self.overlay_always_on_top.is_active();
        cfg.overlay_click_through = self.overlay_click_through.is_active();
        cfg.overlay_position = [
            self.overlay_pos_x.value() as i32,
            self.overlay_pos_y.value() as i32,
        ];
        cfg.overlay_skin = self.overlay_skin.text().to_string();

        cfg.history_retention_days = self.retention_days.value() as u32;
        cfg.history_timezone = self.timezone.text().to_string();

        cfg.plugins_enabled = self.plugins_enabled.is_active();
        cfg.process_traffic_backend =
            combo_selected(&self.process_backend).unwrap_or_else(|| "off".into());
        cfg
    }

    fn rebuild_interface_model(&self, ifaces: &[String]) {
        let model = &self.interface_model;
        model.splice(0, model.n_items(), &[]);
        model.append(AUTO_INTERFACE_LABEL);
        let current = self.config.borrow().active_interface.clone();
        for iface in ifaces {
            model.append(iface);
        }
        self.loading.set(true);
        set_active_interface_row(&self.active_interface, &current);
        self.loading.set(false);
    }

    fn rebuild_items(&self, items: &[UiItem]) {
        let box_ = &self.overlay_items_box;
        while let Some(child) = box_.first_child() {
            box_.remove(&child);
        }
        for item in items {
            let row = adw::SwitchRow::new();
            row.set_title(&item.label);
            row.set_subtitle(&format!("{} · {}", item.id, item.unit));
            row.set_active(item.visible);
            box_.append(&row);
        }
    }

    fn rebuild_alerts(&self, rules: &[UiAlertRule]) {
        let box_ = &self.alerts_box;
        while let Some(child) = box_.first_child() {
            box_.remove(&child);
        }
        if rules.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("暂无告警规则");
            row.set_subtitle("可在配置文件中定义 cpu_pct / mem_pct / net_rx_bps 等规则");
            box_.append(&row);
            return;
        }
        for rule in rules {
            let row = adw::SwitchRow::new();
            row.set_title(&format!("{} {} {}", rule.metric, rule.op, rule.threshold));
            row.set_subtitle(&format!(
                "持续 {}s · 冷却 {}s · {}",
                rule.duration_s,
                rule.cooldown_s,
                if rule.quiet_hours.is_empty() {
                    "无静默时段".to_string()
                } else {
                    format!("静默 {}", rule.quiet_hours)
                }
            ));
            row.set_active(rule.enabled);
            box_.append(&row);
        }
    }

    fn rebuild_collector_status(&self, map: &HashMap<String, OwnedValue>) {
        let box_ = &self.collector_box;
        while let Some(child) = box_.first_child() {
            box_.remove(&child);
        }
        if map.is_empty() {
            let row = adw::ActionRow::new();
            row.set_title("暂无采集器状态");
            box_.append(&row);
            return;
        }
        let mut names: Vec<&String> = map.keys().collect();
        names.sort();
        for name in names {
            let (available, reason) = parse_status_entry(map.get(name).expect("key"));
            let row = adw::ActionRow::new();
            row.set_title(name);
            row.set_subtitle(&if available {
                "可用".to_string()
            } else {
                format!("不可用：{reason}")
            });
            let icon = gtk::Image::from_icon_name(if available {
                "emblem-ok-symbolic"
            } else {
                "dialog-warning-symbolic"
            });
            row.add_prefix(&icon);
            box_.append(&row);
        }
    }
}

fn parse_status_entry(value: &OwnedValue) -> (bool, String) {
    let owned = match value.try_clone() {
        Ok(v) => v,
        Err(_) => return (false, "无法解析状态".into()),
    };
    if let Ok(map) = HashMap::<String, OwnedValue>::try_from(owned) {
        let available = map
            .get("available")
            .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
            .unwrap_or(false);
        let reason = map
            .get("reason")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_else(|| String::from("未知原因"));
        (available, reason)
    } else {
        (false, "无法解析状态".into())
    }
}

pub fn run() {
    let app = adw::Application::builder()
        .application_id("com.trafficview.Settings")
        .build();
    app.connect_activate(on_activate);
    app.run();
}

fn on_activate(app: &adw::Application) {
    let window = adw::ApplicationWindow::new(app);
    window.set_title(Some("TrafficView 设置"));
    window.set_default_size(760, 600);
    gtk::Window::set_default_icon_name("trafficview");

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title = gtk::Label::new(Some("TrafficView 设置"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    let save_button = gtk::Button::with_label("保存");
    save_button.add_css_class("suggested-action");
    header.pack_end(&save_button);

    // 断连横幅：含“重新连接”按钮，不提供任何提权入口。
    let banner = adw::Banner::new("正在连接到 TrafficView 守护进程…");
    banner.set_button_label(Some("重新连接"));
    banner.set_revealed(true);

    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);

    // 主内容：左侧导航 + 右侧页面栈；外面包 ToastOverlay 以便弹出保存提示。
    let toast_overlay = adw::ToastOverlay::new();
    let content_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let stack = gtk::Stack::new();
    let sidebar = gtk::StackSidebar::new();
    sidebar.set_stack(&stack);
    sidebar.set_width_request(180);
    content_box.append(&sidebar);
    content_box.append(&stack);
    toast_overlay.set_child(Some(&content_box));
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));

    // ---- 控件 ----
    let sample_interval =
        adw::SpinRow::new(Some(&adj(1000.0, SAMPLE_MIN, SAMPLE_MAX, 100.0)), 100.0, 0);
    sample_interval.set_title("采样间隔（毫秒）");
    sample_interval.set_subtitle("500–30000，越短占用越高");

    let autostart = adw::SwitchRow::new();
    autostart.set_title("开机自启动");
    autostart.set_subtitle("写入当前用户的 XDG 自启动条目，无需提权");

    let (active_interface, interface_model) = combo(&[AUTO_INTERFACE_LABEL]);
    active_interface.set_title("活动网卡");
    active_interface.set_subtitle("留空表示按默认路由自动选择");

    let blacklist = adw::EntryRow::new();
    blacklist.set_title("网卡黑名单（逗号分隔，支持通配符 lo, docker*, veth*）");

    let whitelist = adw::EntryRow::new();
    whitelist.set_title("网卡白名单（逗号分隔；命中且存在时优先于默认路由）");

    let (smoothing, _) = combo(&["none", "moving_average_3", "ema"]);
    smoothing.set_title("速率平滑");
    smoothing.set_subtitle("none / 三点移动平均 / 指数滑动平均");

    let memory_include_cache = adw::SwitchRow::new();
    memory_include_cache.set_title("内存用量包含缓存");
    memory_include_cache.set_subtitle("关闭时按 MemAvailable 计算可用内存");

    let (theme, _) = combo(&["auto", "light", "dark"]);
    theme.set_title("主题");
    theme.set_subtitle("auto 跟随桌面，light/dark 强制指定");

    let overlay_visible = adw::SwitchRow::new();
    overlay_visible.set_title("显示悬浮窗");

    let overlay_opacity = adw::SpinRow::new(Some(&adj(0.9, OPACITY_MIN, OPACITY_MAX, 0.1)), 0.1, 2);
    overlay_opacity.set_title("透明度");

    let (overlay_units, _) = combo(&["bytes", "bits"]);
    overlay_units.set_title("速率单位");

    let overlay_always_on_top = adw::SwitchRow::new();
    overlay_always_on_top.set_title("窗口置顶");

    let overlay_click_through = adw::SwitchRow::new();
    overlay_click_through.set_title("点击穿透");

    let overlay_pos_x = adw::SpinRow::new(Some(&adj(40.0, -10000.0, 10000.0, 1.0)), 1.0, 0);
    overlay_pos_x.set_title("位置 X");
    let overlay_pos_y = adw::SpinRow::new(Some(&adj(40.0, -10000.0, 10000.0, 1.0)), 1.0, 0);
    overlay_pos_y.set_title("位置 Y");

    let overlay_skin = adw::EntryRow::new();
    overlay_skin.set_title("皮肤目录（空字符串使用内置布局）");

    let overlay_items_box = gtk::ListBox::new();
    overlay_items_box.add_css_class("boxed-list");

    let alerts_box = gtk::ListBox::new();
    alerts_box.add_css_class("boxed-list");

    let retention_days =
        adw::SpinRow::new(Some(&adj(90.0, RETENTION_MIN, RETENTION_MAX, 1.0)), 1.0, 0);
    retention_days.set_title("历史保留天数");
    retention_days.set_subtitle("1–3650；仅影响分钟明细，日汇总永久保留");

    let timezone = adw::EntryRow::new();
    timezone.set_title("时区（空=系统本地，或 UTC / +08:00）");

    let plugins_enabled = adw::SwitchRow::new();
    plugins_enabled.set_title("启用插件");
    plugins_enabled.set_subtitle("插件清单位于 XDG 数据目录，失败不影响内置指标");

    let (process_backend, _) = combo(&["off", "ebpf", "af_packet", "auto"]);
    process_backend.set_title("进程级流量后端");
    process_backend
        .set_subtitle("off=关闭；auto 优先 eBPF 再回退 AF_PACKET；helper 需管理员安装，此处仅配置");

    let collector_box = gtk::ListBox::new();
    collector_box.add_css_class("boxed-list");

    // ---- 组装六个页面 ----
    let general = adw::PreferencesPage::new();
    general.set_title("通用");
    general.set_icon_name(Some("preferences-system-symbolic"));
    let g_general = adw::PreferencesGroup::new();
    g_general.add(&sample_interval);
    g_general.add(&autostart);
    general.add(&g_general);
    stack.add_titled(&page_scroll(&general), Some("general"), "通用");

    let network = adw::PreferencesPage::new();
    network.set_title("网络");
    network.set_icon_name(Some("network-workgroup-symbolic"));
    let g_net = adw::PreferencesGroup::new();
    g_net.set_title("采集");
    g_net.add(&active_interface);
    g_net.add(&smoothing);
    g_net.add(&memory_include_cache);
    let g_net_filter = adw::PreferencesGroup::new();
    g_net_filter.set_title("网卡过滤");
    g_net_filter.add(&blacklist);
    g_net_filter.add(&whitelist);
    network.add(&g_net);
    network.add(&g_net_filter);
    stack.add_titled(&page_scroll(&network), Some("network"), "网络");

    let display = adw::PreferencesPage::new();
    display.set_title("显示");
    display.set_icon_name(Some("preferences-desktop-display-symbolic"));
    let g_theme = adw::PreferencesGroup::new();
    g_theme.add(&theme);
    let g_overlay = adw::PreferencesGroup::new();
    g_overlay.set_title("悬浮窗");
    g_overlay.add(&overlay_visible);
    g_overlay.add(&overlay_opacity);
    g_overlay.add(&overlay_units);
    g_overlay.add(&overlay_always_on_top);
    g_overlay.add(&overlay_click_through);
    g_overlay.add(&overlay_pos_x);
    g_overlay.add(&overlay_pos_y);
    g_overlay.add(&overlay_skin);
    let g_items = adw::PreferencesGroup::new();
    g_items.set_title("显示项");
    g_items.add(&overlay_items_box);
    display.add(&g_theme);
    display.add(&g_overlay);
    display.add(&g_items);
    stack.add_titled(&page_scroll(&display), Some("display"), "显示");

    let alerts = adw::PreferencesPage::new();
    alerts.set_title("告警");
    alerts.set_icon_name(Some("dialog-information-symbolic"));
    let g_alerts = adw::PreferencesGroup::new();
    g_alerts.set_title("规则（只读概览，完整编辑请修改配置文件）");
    g_alerts.add(&alerts_box);
    alerts.add(&g_alerts);
    stack.add_titled(&page_scroll(&alerts), Some("alerts"), "告警");

    let history = adw::PreferencesPage::new();
    history.set_title("历史");
    history.set_icon_name(Some("document-open-recent-symbolic"));
    let g_history = adw::PreferencesGroup::new();
    g_history.add(&retention_days);
    g_history.add(&timezone);
    history.add(&g_history);
    stack.add_titled(&page_scroll(&history), Some("history"), "历史");

    let advanced = adw::PreferencesPage::new();
    advanced.set_title("插件与高级");
    advanced.set_icon_name(Some("preferences-system-symbolic"));
    let g_plugins = adw::PreferencesGroup::new();
    g_plugins.set_title("插件");
    g_plugins.add(&plugins_enabled);
    let g_pt = adw::PreferencesGroup::new();
    g_pt.set_title("进程级流量");
    g_pt.add(&process_backend);
    let g_status = adw::PreferencesGroup::new();
    g_status.set_title("采集器状态");
    g_status.add(&collector_box);
    advanced.add(&g_plugins);
    advanced.add(&g_pt);
    advanced.add(&g_status);
    stack.add_titled(&page_scroll(&advanced), Some("advanced"), "插件与高级");

    // ---- 关于：静态页面，展示图标、版本、功能介绍与项目链接 ----
    let about = adw::PreferencesPage::new();
    about.set_title("关于");
    about.set_icon_name(Some("help-about-symbolic"));

    let g_identity = adw::PreferencesGroup::new();
    let identity = gtk::Box::new(gtk::Orientation::Vertical, 6);
    identity.set_halign(gtk::Align::Center);
    identity.set_margin_top(18);
    identity.set_margin_bottom(18);
    let logo = gtk::Image::from_icon_name("trafficview");
    logo.set_pixel_size(64);
    let app_name = gtk::Label::new(Some("TrafficView"));
    app_name.add_css_class("title-1");
    let version = gtk::Label::new(Some(&format!("版本 {}", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("dim-label");
    identity.append(&logo);
    identity.append(&app_name);
    identity.append(&version);
    g_identity.add(&identity);
    about.add(&g_identity);

    let g_features = adw::PreferencesGroup::new();
    g_features.set_title("功能介绍");
    let features = gtk::Label::new(Some(
        "Linux 桌面实时流量与系统负载监控。悬浮窗显示上下行速率、CPU、内存、GPU 及温度；\
         支持托盘控制、SQLite 历史统计、阈值告警与 waybar 等多前端输出。\
         所有数据由守护进程采集并经 D-Bus 分发。",
    ));
    features.set_wrap(true);
    features.set_justify(gtk::Justification::Left);
    g_features.add(&features);
    about.add(&g_features);

    let g_links = adw::PreferencesGroup::new();
    g_links.set_title("项目链接");
    let github =
        gtk::LinkButton::with_label("https://github.com/umgsai/TrafficView", "GitHub 项目主页");
    github.set_halign(gtk::Align::Start);
    g_links.add(&github);
    about.add(&g_links);
    stack.add_titled(&page_scroll(&about), Some("about"), "关于");

    // ---- 可编辑控件清单：断连时统一禁用 ----
    let editable: Vec<gtk::Widget> = vec![
        sample_interval.clone().upcast(),
        autostart.clone().upcast(),
        active_interface.clone().upcast(),
        blacklist.clone().upcast(),
        whitelist.clone().upcast(),
        smoothing.clone().upcast(),
        memory_include_cache.clone().upcast(),
        theme.clone().upcast(),
        overlay_visible.clone().upcast(),
        overlay_opacity.clone().upcast(),
        overlay_units.clone().upcast(),
        overlay_always_on_top.clone().upcast(),
        overlay_click_through.clone().upcast(),
        overlay_pos_x.clone().upcast(),
        overlay_pos_y.clone().upcast(),
        overlay_skin.clone().upcast(),
        retention_days.clone().upcast(),
        timezone.clone().upcast(),
        plugins_enabled.clone().upcast(),
        process_backend.clone().upcast(),
    ];

    // ---- 状态与 D-Bus ----
    let (event_tx, event_rx) = async_channel::unbounded::<ClientEvent>();
    let client = DaemonClient::start(event_tx);
    let state = Rc::new(RefCell::new(AppState {
        client,
        config: RefCell::new(UiConfig::default()),
        dirty: Cell::new(false),
        loading: Cell::new(false),
        connected: Cell::new(false),
        banner: banner.clone(),
        toast_overlay: toast_overlay.clone(),
        sample_interval: sample_interval.clone(),
        autostart: autostart.clone(),
        active_interface: active_interface.clone(),
        interface_model: interface_model.clone(),
        blacklist: blacklist.clone(),
        whitelist: whitelist.clone(),
        smoothing: smoothing.clone(),
        memory_include_cache: memory_include_cache.clone(),
        theme: theme.clone(),
        overlay_visible: overlay_visible.clone(),
        overlay_opacity: overlay_opacity.clone(),
        overlay_units: overlay_units.clone(),
        overlay_always_on_top: overlay_always_on_top.clone(),
        overlay_click_through: overlay_click_through.clone(),
        overlay_pos_x: overlay_pos_x.clone(),
        overlay_pos_y: overlay_pos_y.clone(),
        overlay_skin: overlay_skin.clone(),
        overlay_items_box: overlay_items_box.clone(),
        alerts_box: alerts_box.clone(),
        retention_days: retention_days.clone(),
        timezone: timezone.clone(),
        plugins_enabled: plugins_enabled.clone(),
        process_backend: process_backend.clone(),
        collector_box: collector_box.clone(),
        editable: editable.clone(),
    }));

    // 初始即禁用，等待 Connected 事件。
    for widget in &editable {
        widget.set_sensitive(false);
    }

    // 事件派发到主线程。
    let state_for_loop = state.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = event_rx.recv().await {
            state_for_loop.borrow().handle_event(event);
        }
    });

    // 控件变更标记 dirty。
    wire_dirty(&state, &sample_interval, "value");
    wire_dirty(&state, &blacklist, "text");
    wire_dirty(&state, &whitelist, "text");
    wire_dirty(&state, &smoothing, "selected");
    wire_dirty(&state, &memory_include_cache, "active");
    wire_dirty(&state, &theme, "selected");
    wire_dirty(&state, &overlay_visible, "active");
    wire_dirty(&state, &overlay_opacity, "value");
    wire_dirty(&state, &overlay_units, "selected");
    wire_dirty(&state, &overlay_always_on_top, "active");
    wire_dirty(&state, &overlay_click_through, "active");
    wire_dirty(&state, &overlay_pos_x, "value");
    wire_dirty(&state, &overlay_pos_y, "value");
    wire_dirty(&state, &overlay_skin, "text");
    wire_dirty(&state, &retention_days, "value");
    wire_dirty(&state, &timezone, "text");
    wire_dirty(&state, &plugins_enabled, "active");
    wire_dirty(&state, &process_backend, "selected");
    wire_dirty(&state, &active_interface, "selected");

    // autostart 是本地文件，切换立即写盘（不走 UpdateConfig）。
    {
        let state = state.clone();
        autostart.connect_active_notify(move |row| {
            let st = state.borrow();
            if st.loading.get() {
                return;
            }
            if let Err(error) = autostart::set_enabled(row.is_active()) {
                st.show_toast(&format!("自启动设置失败：{error}"));
            }
        });
    }

    // 保存按钮：一次性提交完整配置。
    {
        let state = state.clone();
        save_button.connect_clicked(move |_| {
            let st = state.borrow();
            if !st.connected.get() {
                st.show_toast("未连接到守护进程，无法保存");
                return;
            }
            let cfg = st.collect_config();
            st.client.update_config(&cfg);
        });
    }

    // 横幅“重新连接”按钮：触发一次刷新。
    {
        let state = state.clone();
        banner.connect_button_clicked(move |_| {
            state.borrow().client.refresh();
        });
    }

    window.present();
}

fn page_scroll(page: &adw::PreferencesPage) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(page));
    scroll
}

fn wire_dirty<T: glib::object::IsA<glib::Object> + Clone + 'static>(
    state: &Rc<RefCell<AppState>>,
    widget: &T,
    property: &'static str,
) {
    let state = state.clone();
    widget.connect_notify_local(Some(property), move |_, _| {
        state.borrow().mark_dirty();
    });
}

#[cfg(test)]
mod tests {
    use super::interface_value;

    #[test]
    fn auto_interface_maps_to_empty_string() {
        assert_eq!(interface_value("（自动选择默认网卡）"), "");
    }

    #[test]
    fn real_interface_is_kept() {
        assert_eq!(interface_value("enp0s3"), "enp0s3");
    }
}
