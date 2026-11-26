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
use trafficview_core::config::ConfigStore;
use trafficview_core::i18n::{self, tr, trf, Language};
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
const LANGUAGE_VALUES: &[&str] = &["auto", "zh", "en"];
const SMOOTHING_VALUES: &[&str] = &["none", "moving_average_3", "ema"];
const THEME_VALUES: &[&str] = &["auto", "light", "dark"];
const OVERLAY_UNIT_VALUES: &[&str] = &["bytes", "bits"];
const PROCESS_BACKEND_VALUES: &[&str] = &["off", "ebpf", "af_packet", "auto"];

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

fn combo_selected_value(row: &adw::ComboRow, values: &[&str]) -> Option<String> {
    values
        .get(row.selected() as usize)
        .map(|value| (*value).to_string())
}

fn combo_set_value(row: &adw::ComboRow, values: &[&str], value: &str) {
    if let Some(index) = values.iter().position(|candidate| *candidate == value) {
        row.set_selected(index as u32);
    }
}

/// “自动选择”在 D-Bus 契约里是空字符串，只在界面上显示成文案。
fn interface_value(selected: &str) -> String {
    if selected == tr(Language::Zh, "settings.network.active_interface.auto")
        || selected == tr(Language::En, "settings.network.active_interface.auto")
    {
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
    lang: Language,
    dirty: Cell<bool>,
    loading: Cell<bool>,
    connected: Cell<bool>,
    pending_language_reopen_hint: Cell<bool>,

    banner: adw::Banner,
    toast_overlay: adw::ToastOverlay,

    // General
    language: adw::ComboRow,
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
                    self.show_toast(tr(self.lang, "settings.saved"));
                    if self.pending_language_reopen_hint.replace(false) {
                        self.show_toast(tr(self.lang, "settings.language_reopen_hint"));
                    }
                }
                Err(reason) => {
                    self.pending_language_reopen_hint.set(false);
                    self.show_toast(&trf(
                        self.lang,
                        "settings.save_failed",
                        &[("reason", &reason)],
                    ));
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
            self.banner.set_title(&trf(
                self.lang,
                "settings.connection_failed",
                &[("reason", reason)],
            ));
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
        combo_set_value(&self.language, LANGUAGE_VALUES, &cfg.language);
        self.sample_interval
            .set_value(f64::from(cfg.sample_interval_ms));
        self.autostart.set_active(autostart::is_enabled());

        set_active_interface_row(&self.active_interface, &cfg.active_interface);
        self.blacklist.set_text(&join_csv(&cfg.interface_blacklist));
        self.whitelist.set_text(&join_csv(&cfg.interface_whitelist));
        combo_set_value(&self.smoothing, SMOOTHING_VALUES, &cfg.network_smoothing);
        self.memory_include_cache
            .set_active(cfg.memory_include_cache);

        combo_set_value(&self.theme, THEME_VALUES, &cfg.theme);
        self.overlay_visible.set_active(cfg.overlay_visible);
        self.overlay_opacity.set_value(cfg.overlay_opacity);
        combo_set_value(&self.overlay_units, OVERLAY_UNIT_VALUES, &cfg.overlay_units);
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
        combo_set_value(
            &self.process_backend,
            PROCESS_BACKEND_VALUES,
            &cfg.process_traffic_backend,
        );
        self.loading.set(false);
    }

    /// 从控件读取当前值，组装成待提交的配置快照。
    fn collect_config(&self) -> UiConfig {
        let mut cfg = self.config.borrow().clone();
        cfg.language =
            combo_selected_value(&self.language, LANGUAGE_VALUES).unwrap_or_else(|| "auto".into());
        cfg.sample_interval_ms = self.sample_interval.value() as u32;
        cfg.active_interface =
            interface_value(&combo_selected(&self.active_interface).unwrap_or_default());
        cfg.interface_blacklist = split_csv(&self.blacklist.text());
        cfg.interface_whitelist = split_csv(&self.whitelist.text());
        cfg.network_smoothing = combo_selected_value(&self.smoothing, SMOOTHING_VALUES)
            .unwrap_or_else(|| "none".into());
        cfg.memory_include_cache = self.memory_include_cache.is_active();

        cfg.theme =
            combo_selected_value(&self.theme, THEME_VALUES).unwrap_or_else(|| "dark".into());
        cfg.overlay_visible = self.overlay_visible.is_active();
        cfg.overlay_opacity = self.overlay_opacity.value();
        cfg.overlay_units = combo_selected_value(&self.overlay_units, OVERLAY_UNIT_VALUES)
            .unwrap_or_else(|| "bytes".into());
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
            combo_selected_value(&self.process_backend, PROCESS_BACKEND_VALUES)
                .unwrap_or_else(|| "off".into());
        cfg
    }

    fn rebuild_interface_model(&self, ifaces: &[String]) {
        let model = &self.interface_model;
        model.splice(0, model.n_items(), &[]);
        model.append(tr(self.lang, "settings.network.active_interface.auto"));
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
            row.set_subtitle(&trf(
                self.lang,
                "settings.display.item_details",
                &[("id", &item.id), ("unit", &item.unit)],
            ));
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
            row.set_title(tr(self.lang, "settings.alerts.empty_title"));
            row.set_subtitle(tr(self.lang, "settings.alerts.empty_subtitle"));
            box_.append(&row);
            return;
        }
        for rule in rules {
            let row = adw::SwitchRow::new();
            let threshold = rule.threshold.to_string();
            row.set_title(&trf(
                self.lang,
                "settings.alerts.rule_title",
                &[
                    ("metric", &rule.metric),
                    ("op", &rule.op),
                    ("threshold", &threshold),
                ],
            ));
            let duration = rule.duration_s.to_string();
            let cooldown = rule.cooldown_s.to_string();
            let quiet = if rule.quiet_hours.is_empty() {
                tr(self.lang, "settings.alerts.no_quiet_hours").to_string()
            } else {
                trf(
                    self.lang,
                    "settings.alerts.quiet_hours",
                    &[("hours", &rule.quiet_hours)],
                )
            };
            row.set_subtitle(&trf(
                self.lang,
                "settings.alerts.rule_subtitle",
                &[
                    ("duration", &duration),
                    ("cooldown", &cooldown),
                    ("quiet", &quiet),
                ],
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
            row.set_title(tr(self.lang, "settings.advanced.collector_empty"));
            box_.append(&row);
            return;
        }
        let mut names: Vec<&String> = map.keys().collect();
        names.sort();
        for name in names {
            let (available, reason) = parse_status_entry(self.lang, map.get(name).expect("key"));
            let row = adw::ActionRow::new();
            row.set_title(name);
            row.set_subtitle(&if available {
                tr(self.lang, "settings.advanced.collector_available").to_string()
            } else {
                trf(
                    self.lang,
                    "settings.advanced.collector_unavailable",
                    &[("reason", &reason)],
                )
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

fn parse_status_entry(lang: Language, value: &OwnedValue) -> (bool, String) {
    let owned = match value.try_clone() {
        Ok(v) => v,
        Err(_) => {
            return (
                false,
                tr(lang, "settings.advanced.collector_parse_error").into(),
            );
        }
    };
    if let Ok(map) = HashMap::<String, OwnedValue>::try_from(owned) {
        let available = map
            .get("available")
            .and_then(|v| bool::try_from(v.try_clone().ok()?).ok())
            .unwrap_or(false);
        let reason = map
            .get("reason")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_else(|| tr(lang, "settings.advanced.collector_unknown_reason").into());
        (available, reason)
    } else {
        (
            false,
            tr(lang, "settings.advanced.collector_parse_error").into(),
        )
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
    let initial_config = ConfigStore::xdg()
        .ok()
        .and_then(|store| store.load().ok())
        .unwrap_or_default();
    let lang = i18n::resolve(&initial_config.language);
    let window = adw::ApplicationWindow::new(app);
    window.set_title(Some(tr(lang, "settings.window_title")));
    window.set_default_size(760, 600);
    gtk::Window::set_default_icon_name("trafficview");

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let title = gtk::Label::new(Some(tr(lang, "settings.window_title")));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    let save_button = gtk::Button::with_label(tr(lang, "settings.save"));
    save_button.add_css_class("suggested-action");
    header.pack_end(&save_button);

    // 断连横幅：含“重新连接”按钮，不提供任何提权入口。
    let banner = adw::Banner::new(tr(lang, "settings.connecting"));
    banner.set_button_label(Some(tr(lang, "settings.reconnect")));
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
    let (language, _) = combo(&[
        tr(lang, "settings.general.language.auto"),
        "中文",
        "English",
    ]);
    language.set_title(tr(lang, "settings.general.language.title"));
    language.set_subtitle(tr(lang, "settings.general.language.subtitle"));

    let sample_interval =
        adw::SpinRow::new(Some(&adj(1000.0, SAMPLE_MIN, SAMPLE_MAX, 100.0)), 100.0, 0);
    sample_interval.set_title(tr(lang, "settings.general.sample_interval.title"));
    sample_interval.set_subtitle(tr(lang, "settings.general.sample_interval.subtitle"));

    let autostart = adw::SwitchRow::new();
    autostart.set_title(tr(lang, "settings.general.autostart.title"));
    autostart.set_subtitle(tr(lang, "settings.general.autostart.subtitle"));

    let (active_interface, interface_model) =
        combo(&[tr(lang, "settings.network.active_interface.auto")]);
    active_interface.set_title(tr(lang, "settings.network.active_interface.title"));
    active_interface.set_subtitle(tr(lang, "settings.network.active_interface.subtitle"));

    let blacklist = adw::EntryRow::new();
    blacklist.set_title(tr(lang, "settings.network.blacklist.title"));

    let whitelist = adw::EntryRow::new();
    whitelist.set_title(tr(lang, "settings.network.whitelist.title"));

    let (smoothing, _) = combo(&[
        tr(lang, "settings.network.smoothing.none"),
        tr(lang, "settings.network.smoothing.moving_average_3"),
        tr(lang, "settings.network.smoothing.ema"),
    ]);
    smoothing.set_title(tr(lang, "settings.network.smoothing.title"));
    smoothing.set_subtitle(tr(lang, "settings.network.smoothing.subtitle"));

    let memory_include_cache = adw::SwitchRow::new();
    memory_include_cache.set_title(tr(lang, "settings.network.memory_include_cache.title"));
    memory_include_cache.set_subtitle(tr(lang, "settings.network.memory_include_cache.subtitle"));

    let (theme, _) = combo(&[
        tr(lang, "settings.display.theme.auto"),
        tr(lang, "settings.display.theme.light"),
        tr(lang, "settings.display.theme.dark"),
    ]);
    theme.set_title(tr(lang, "settings.display.theme.title"));
    theme.set_subtitle(tr(lang, "settings.display.theme.subtitle"));

    let overlay_visible = adw::SwitchRow::new();
    overlay_visible.set_title(tr(lang, "settings.display.overlay_visible.title"));

    let overlay_opacity = adw::SpinRow::new(Some(&adj(0.9, OPACITY_MIN, OPACITY_MAX, 0.1)), 0.1, 2);
    overlay_opacity.set_title(tr(lang, "settings.display.overlay_opacity.title"));

    let (overlay_units, _) = combo(&[
        tr(lang, "settings.display.overlay_units.bytes"),
        tr(lang, "settings.display.overlay_units.bits"),
    ]);
    overlay_units.set_title(tr(lang, "settings.display.overlay_units.title"));

    let overlay_always_on_top = adw::SwitchRow::new();
    overlay_always_on_top.set_title(tr(lang, "settings.display.overlay_always_on_top.title"));

    let overlay_click_through = adw::SwitchRow::new();
    overlay_click_through.set_title(tr(lang, "settings.display.overlay_click_through.title"));

    let overlay_pos_x = adw::SpinRow::new(Some(&adj(40.0, -10000.0, 10000.0, 1.0)), 1.0, 0);
    overlay_pos_x.set_title(tr(lang, "settings.display.overlay_pos_x.title"));
    let overlay_pos_y = adw::SpinRow::new(Some(&adj(40.0, -10000.0, 10000.0, 1.0)), 1.0, 0);
    overlay_pos_y.set_title(tr(lang, "settings.display.overlay_pos_y.title"));

    let overlay_skin = adw::EntryRow::new();
    overlay_skin.set_title(tr(lang, "settings.display.overlay_skin.title"));

    let overlay_items_box = gtk::ListBox::new();
    overlay_items_box.add_css_class("boxed-list");

    let alerts_box = gtk::ListBox::new();
    alerts_box.add_css_class("boxed-list");

    let retention_days =
        adw::SpinRow::new(Some(&adj(90.0, RETENTION_MIN, RETENTION_MAX, 1.0)), 1.0, 0);
    retention_days.set_title(tr(lang, "settings.history.retention_days.title"));
    retention_days.set_subtitle(tr(lang, "settings.history.retention_days.subtitle"));

    let timezone = adw::EntryRow::new();
    timezone.set_title(tr(lang, "settings.history.timezone.title"));

    let plugins_enabled = adw::SwitchRow::new();
    plugins_enabled.set_title(tr(lang, "settings.advanced.plugins_enabled.title"));
    plugins_enabled.set_subtitle(tr(lang, "settings.advanced.plugins_enabled.subtitle"));

    let (process_backend, _) = combo(&[
        tr(lang, "settings.advanced.process_backend.off"),
        tr(lang, "settings.advanced.process_backend.ebpf"),
        tr(lang, "settings.advanced.process_backend.af_packet"),
        tr(lang, "settings.advanced.process_backend.auto"),
    ]);
    process_backend.set_title(tr(lang, "settings.advanced.process_backend.title"));
    process_backend.set_subtitle(tr(lang, "settings.advanced.process_backend.subtitle"));

    let collector_box = gtk::ListBox::new();
    collector_box.add_css_class("boxed-list");

    // ---- 组装六个页面 ----
    let general = adw::PreferencesPage::new();
    general.set_title(tr(lang, "settings.general.page"));
    general.set_icon_name(Some("preferences-system-symbolic"));
    let g_general = adw::PreferencesGroup::new();
    g_general.add(&language);
    g_general.add(&sample_interval);
    g_general.add(&autostart);
    general.add(&g_general);
    stack.add_titled(
        &page_scroll(&general),
        Some("general"),
        tr(lang, "settings.general.page"),
    );

    let network = adw::PreferencesPage::new();
    network.set_title(tr(lang, "settings.network.page"));
    network.set_icon_name(Some("network-workgroup-symbolic"));
    let g_net = adw::PreferencesGroup::new();
    g_net.set_title(tr(lang, "settings.network.group_collection"));
    g_net.add(&active_interface);
    g_net.add(&smoothing);
    g_net.add(&memory_include_cache);
    let g_net_filter = adw::PreferencesGroup::new();
    g_net_filter.set_title(tr(lang, "settings.network.group_filter"));
    g_net_filter.add(&blacklist);
    g_net_filter.add(&whitelist);
    network.add(&g_net);
    network.add(&g_net_filter);
    stack.add_titled(
        &page_scroll(&network),
        Some("network"),
        tr(lang, "settings.network.page"),
    );

    let display = adw::PreferencesPage::new();
    display.set_title(tr(lang, "settings.display.page"));
    display.set_icon_name(Some("preferences-desktop-display-symbolic"));
    let g_theme = adw::PreferencesGroup::new();
    g_theme.add(&theme);
    let g_overlay = adw::PreferencesGroup::new();
    g_overlay.set_title(tr(lang, "settings.display.group_overlay"));
    g_overlay.add(&overlay_visible);
    g_overlay.add(&overlay_opacity);
    g_overlay.add(&overlay_units);
    g_overlay.add(&overlay_always_on_top);
    g_overlay.add(&overlay_click_through);
    g_overlay.add(&overlay_pos_x);
    g_overlay.add(&overlay_pos_y);
    g_overlay.add(&overlay_skin);
    let g_items = adw::PreferencesGroup::new();
    g_items.set_title(tr(lang, "settings.display.group_items"));
    g_items.add(&overlay_items_box);
    display.add(&g_theme);
    display.add(&g_overlay);
    display.add(&g_items);
    stack.add_titled(
        &page_scroll(&display),
        Some("display"),
        tr(lang, "settings.display.page"),
    );

    let alerts = adw::PreferencesPage::new();
    alerts.set_title(tr(lang, "settings.alerts.page"));
    alerts.set_icon_name(Some("dialog-information-symbolic"));
    let g_alerts = adw::PreferencesGroup::new();
    g_alerts.set_title(tr(lang, "settings.alerts.group_rules"));
    g_alerts.add(&alerts_box);
    alerts.add(&g_alerts);
    stack.add_titled(
        &page_scroll(&alerts),
        Some("alerts"),
        tr(lang, "settings.alerts.page"),
    );

    let history = adw::PreferencesPage::new();
    history.set_title(tr(lang, "settings.history.page"));
    history.set_icon_name(Some("document-open-recent-symbolic"));
    let g_history = adw::PreferencesGroup::new();
    g_history.add(&retention_days);
    g_history.add(&timezone);
    history.add(&g_history);
    stack.add_titled(
        &page_scroll(&history),
        Some("history"),
        tr(lang, "settings.history.page"),
    );

    let advanced = adw::PreferencesPage::new();
    advanced.set_title(tr(lang, "settings.advanced.page"));
    advanced.set_icon_name(Some("preferences-system-symbolic"));
    let g_plugins = adw::PreferencesGroup::new();
    g_plugins.set_title(tr(lang, "settings.advanced.group_plugins"));
    g_plugins.add(&plugins_enabled);
    let g_pt = adw::PreferencesGroup::new();
    g_pt.set_title(tr(lang, "settings.advanced.group_process"));
    g_pt.add(&process_backend);
    let g_status = adw::PreferencesGroup::new();
    g_status.set_title(tr(lang, "settings.advanced.group_collector"));
    g_status.add(&collector_box);
    advanced.add(&g_plugins);
    advanced.add(&g_pt);
    advanced.add(&g_status);
    stack.add_titled(
        &page_scroll(&advanced),
        Some("advanced"),
        tr(lang, "settings.advanced.page"),
    );

    // ---- 关于：静态页面，展示图标、版本、功能介绍与项目链接 ----
    let about = adw::PreferencesPage::new();
    about.set_title(tr(lang, "settings.about.page"));
    about.set_icon_name(Some("help-about-symbolic"));

    let g_identity = adw::PreferencesGroup::new();
    let identity = gtk::Box::new(gtk::Orientation::Vertical, 6);
    identity.set_halign(gtk::Align::Center);
    identity.set_margin_top(18);
    identity.set_margin_bottom(18);
    let logo = gtk::Image::from_icon_name("trafficview");
    logo.set_pixel_size(64);
    let app_name = gtk::Label::new(Some(tr(lang, "settings.about.app_name")));
    app_name.add_css_class("title-1");
    let version = gtk::Label::new(Some(&trf(
        lang,
        "settings.about.version",
        &[("version", env!("CARGO_PKG_VERSION"))],
    )));
    version.add_css_class("dim-label");
    identity.append(&logo);
    identity.append(&app_name);
    identity.append(&version);
    g_identity.add(&identity);
    about.add(&g_identity);

    let g_features = adw::PreferencesGroup::new();
    g_features.set_title(tr(lang, "settings.about.group_features"));
    let features = gtk::Label::new(Some(tr(lang, "settings.about.features")));
    features.set_wrap(true);
    features.set_justify(gtk::Justification::Left);
    g_features.add(&features);
    about.add(&g_features);

    let g_links = adw::PreferencesGroup::new();
    g_links.set_title(tr(lang, "settings.about.group_links"));
    let github = gtk::LinkButton::with_label(
        "https://github.com/umgsai/TrafficView",
        tr(lang, "settings.about.github"),
    );
    github.set_halign(gtk::Align::Start);
    g_links.add(&github);
    about.add(&g_links);
    stack.add_titled(
        &page_scroll(&about),
        Some("about"),
        tr(lang, "settings.about.page"),
    );

    // ---- 可编辑控件清单：断连时统一禁用 ----
    let editable: Vec<gtk::Widget> = vec![
        language.clone().upcast(),
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
        lang,
        dirty: Cell::new(false),
        loading: Cell::new(false),
        connected: Cell::new(false),
        pending_language_reopen_hint: Cell::new(false),
        banner: banner.clone(),
        toast_overlay: toast_overlay.clone(),
        language: language.clone(),
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
    wire_dirty(&state, &language, "selected");
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
                st.show_toast(&trf(
                    st.lang,
                    "settings.autostart_failed",
                    &[("error", &error.to_string())],
                ));
            }
        });
    }

    // 保存按钮：一次性提交完整配置。
    {
        let state = state.clone();
        save_button.connect_clicked(move |_| {
            let st = state.borrow();
            if !st.connected.get() {
                st.show_toast(tr(st.lang, "settings.disconnected_save"));
                return;
            }
            let previous_language = st.config.borrow().language.clone();
            let cfg = st.collect_config();
            st.pending_language_reopen_hint
                .set(cfg.language != previous_language);
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
    fn english_auto_interface_maps_to_empty_string() {
        assert_eq!(
            interface_value("(Select the default interface automatically)"),
            ""
        );
    }

    #[test]
    fn real_interface_is_kept() {
        assert_eq!(interface_value("enp0s3"), "enp0s3");
    }
}
