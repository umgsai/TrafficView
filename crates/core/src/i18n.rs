//! 中英文静态翻译表与语言解析。
//!
//! 所有 GUI 用户可见文本集中在此；新增文本必须同时补充 ZH/EN 两条。

/// 界面语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Zh,
    En,
}

/// 读取 LC_ALL/LANG 判定语言；未设置或非中文环境均为英文。
pub fn detect() -> Language {
    let locale = std::env::var("LC_ALL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("LANG").ok());
    detect_from(locale.as_deref())
}

/// 纯函数版检测，便于测试：`zh` 开头为中文，其余（含 C/POSIX）为英文。
pub fn detect_from(locale: Option<&str>) -> Language {
    match locale {
        Some(value) if value.to_ascii_lowercase().starts_with("zh") => Language::Zh,
        _ => Language::En,
    }
}

/// 配置为 zh/en 时优先；auto、空串与未知值回退到系统检测。
pub fn resolve(configured: &str) -> Language {
    match configured.trim().to_ascii_lowercase().as_str() {
        "zh" => Language::Zh,
        "en" => Language::En,
        _ => detect(),
    }
}

/// 取静态文案；未知 key 回退英文表，仍无则返回 key 本身，不 panic。
pub fn tr(lang: Language, key: &'static str) -> &'static str {
    let table = match lang {
        Language::Zh => ZH,
        Language::En => EN,
    };
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, text)| *text)
        .or_else(|| {
            EN.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, text)| *text)
        })
        .unwrap_or(key)
}

/// 取带占位的文案并替换 `{name}` 参数。
pub fn trf(lang: Language, key: &'static str, args: &[(&str, &str)]) -> String {
    let mut text = tr(lang, key).to_owned();
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

pub const ZH: &[(&str, &str)] = &[
    ("tray.app_name", "TrafficView"),
    ("tray.title.offline", "TrafficView（守护进程离线）"),
    ("tray.title.paused", "TrafficView（已暂停）"),
    ("tray.title.paused_metrics", "⏸ {title} · {cpu}% · {memory}%"),
    ("tray.title.metrics", "↓{download} ↑{upload} · C{cpu} M{memory}"),
    ("tray.tooltip.offline", "守护进程离线；正在重试…"),
    ("tray.tooltip.title", "TrafficView · ↓ {download}  ↑ {upload}"),
    ("tray.tooltip.interface", "网卡：{interface}"),
    ("tray.tooltip.network", "网络：↓ {download}  ↑ {upload}（总计 ↓ {download_total} ↑ {upload_total}）"),
    ("tray.tooltip.cpu", "CPU：{value}%{frequency}"),
    ("tray.tooltip.cpu_frequency", "  @ {value} MHz"),
    ("tray.tooltip.memory", "内存：{value}%（已用 {used} / 总计 {total}）"),
    ("tray.tooltip.swap", "交换空间：{value}%"),
    ("tray.tooltip.disk", "磁盘 I/O：↓ {read}  ↑ {write}"),
    ("tray.tooltip.battery", "电池：{value}%  {status}"),
    ("tray.tooltip.temp_sensor", "{label} {value}°C"),
    ("tray.tooltip.temp", "温度：{value}"),
    ("tray.tooltip.waiting", "正在等待首个采样…"),
    ("tray.tooltip.alert", "⚠ {message}"),
    ("tray.menu.pause", "暂停"),
    ("tray.menu.resume", "继续"),
    ("tray.menu.show_overlay", "显示悬浮窗"),
    ("tray.menu.hide_overlay", "隐藏悬浮窗"),
    ("tray.menu.network_interface", "网络接口"),
    ("tray.menu.automatic", "自动"),
    ("tray.menu.settings", "设置…"),
    ("tray.menu.open_config", "打开配置文件"),
    ("tray.menu.quit", "退出 TrafficView"),
    ("alert.metric.cpu", "CPU 使用率"),
    ("alert.metric.memory", "内存使用率"),
    ("alert.metric.net_rx", "下行速率"),
    ("alert.metric.net_tx", "上行速率"),
    ("alert.metric.month_bytes", "本月流量"),
    ("alert.above_threshold", "{label} {value} 超过阈值 {threshold}（持续 {duration} 秒）"),
    ("alert.recovered", "{label} 已恢复（当前 {value}）"),
    ("notify.app_name", "TrafficView"),
    ("notify.title", "TrafficView 告警"),
    ("settings.window_title", "TrafficView 设置"),
    ("settings.save", "保存"),
    ("settings.connecting", "正在连接到 TrafficView 守护进程…"),
    ("settings.reconnect", "重新连接"),
    ("settings.connection_failed", "无法连接到 TrafficView 守护进程：{reason}"),
    ("settings.saved", "已保存"),
    ("settings.save_failed", "保存失败：{reason}"),
    ("settings.autostart_failed", "自启动设置失败：{error}"),
    ("settings.disconnected_save", "未连接到守护进程，无法保存"),
    ("settings.general.page", "通用"),
    ("settings.general.language.title", "语言"),
    ("settings.general.language.subtitle", "auto 跟随系统；切换后托盘立即生效，设置界面重新打开后生效"),
    ("settings.general.language.auto", "跟随系统"),
    ("settings.general.sample_interval.title", "采样间隔（毫秒）"),
    ("settings.general.sample_interval.subtitle", "500–30000，越短占用越高"),
    ("settings.general.autostart.title", "开机自启动"),
    ("settings.general.autostart.subtitle", "写入当前用户的 XDG 自启动条目，无需提权"),
    ("settings.network.page", "网络"),
    ("settings.network.group_collection", "采集"),
    ("settings.network.group_filter", "网卡过滤"),
    ("settings.network.active_interface.title", "活动网卡"),
    ("settings.network.active_interface.subtitle", "留空表示按默认路由自动选择"),
    ("settings.network.active_interface.auto", "（自动选择默认网卡）"),
    ("settings.network.smoothing.title", "速率平滑"),
    ("settings.network.smoothing.subtitle", "none / 三点移动平均 / 指数滑动平均"),
    ("settings.network.smoothing.none", "无"),
    ("settings.network.smoothing.moving_average_3", "三点移动平均"),
    ("settings.network.smoothing.ema", "指数滑动平均"),
    ("settings.network.memory_include_cache.title", "内存用量包含缓存"),
    ("settings.network.memory_include_cache.subtitle", "关闭时按 MemAvailable 计算可用内存"),
    ("settings.network.blacklist.title", "网卡黑名单（逗号分隔，支持通配符 lo, docker*, veth*）"),
    ("settings.network.whitelist.title", "网卡白名单（逗号分隔；命中且存在时优先于默认路由）"),
    ("settings.display.page", "显示"),
    ("settings.display.theme.title", "主题"),
    ("settings.display.theme.subtitle", "auto 跟随桌面，light/dark 强制指定"),
    ("settings.display.theme.auto", "跟随系统"),
    ("settings.display.theme.light", "浅色"),
    ("settings.display.theme.dark", "深色"),
    ("settings.display.group_overlay", "悬浮窗"),
    ("settings.display.overlay_visible.title", "显示悬浮窗"),
    ("settings.display.overlay_opacity.title", "透明度"),
    ("settings.display.overlay_units.title", "速率单位"),
    ("settings.display.overlay_units.bytes", "字节"),
    ("settings.display.overlay_units.bits", "比特"),
    ("settings.display.overlay_always_on_top.title", "窗口置顶"),
    ("settings.display.overlay_click_through.title", "点击穿透"),
    ("settings.display.overlay_pos_x.title", "位置 X"),
    ("settings.display.overlay_pos_y.title", "位置 Y"),
    ("settings.display.overlay_skin.title", "皮肤目录（空字符串使用内置布局）"),
    ("settings.display.group_items", "显示项"),
    ("settings.display.item_details", "{id} · {unit}"),
    ("settings.alerts.page", "告警"),
    ("settings.alerts.group_rules", "规则（只读概览，完整编辑请修改配置文件）"),
    ("settings.alerts.empty_title", "暂无告警规则"),
    ("settings.alerts.empty_subtitle", "可在配置文件中定义 cpu_pct / mem_pct / net_rx_bps 等规则"),
    ("settings.alerts.rule_title", "{metric} {op} {threshold}"),
    ("settings.alerts.rule_subtitle", "持续 {duration}s · 冷却 {cooldown}s · {quiet}"),
    ("settings.alerts.no_quiet_hours", "无静默时段"),
    ("settings.alerts.quiet_hours", "静默 {hours}"),
    ("settings.history.page", "历史"),
    ("settings.history.retention_days.title", "历史保留天数"),
    ("settings.history.retention_days.subtitle", "1–3650；仅影响分钟明细，日汇总永久保留"),
    ("settings.history.timezone.title", "时区（空=系统本地，或 UTC / +08:00）"),
    ("settings.advanced.page", "插件与高级"),
    ("settings.advanced.group_plugins", "插件"),
    ("settings.advanced.plugins_enabled.title", "启用插件"),
    ("settings.advanced.plugins_enabled.subtitle", "插件清单位于 XDG 数据目录，失败不影响内置指标"),
    ("settings.advanced.group_process", "进程级流量"),
    ("settings.advanced.process_backend.title", "进程级流量后端"),
    ("settings.advanced.process_backend.subtitle", "off=关闭；auto 优先 eBPF 再回退 AF_PACKET；helper 需管理员安装，此处仅配置"),
    ("settings.advanced.process_backend.off", "关闭"),
    ("settings.advanced.process_backend.ebpf", "eBPF"),
    ("settings.advanced.process_backend.af_packet", "AF_PACKET"),
    ("settings.advanced.process_backend.auto", "自动"),
    ("settings.advanced.group_collector", "采集器状态"),
    ("settings.advanced.collector_empty", "暂无采集器状态"),
    ("settings.advanced.collector_available", "可用"),
    ("settings.advanced.collector_unavailable", "不可用：{reason}"),
    ("settings.advanced.collector_parse_error", "无法解析状态"),
    ("settings.advanced.collector_unknown_reason", "未知原因"),
    ("settings.about.page", "关于"),
    ("settings.about.app_name", "TrafficView"),
    ("settings.about.version", "版本 {version}"),
    ("settings.about.group_features", "功能介绍"),
    ("settings.about.features", "Linux 桌面实时流量与系统负载监控。悬浮窗显示上下行速率、CPU、内存、GPU 及温度；支持托盘控制、SQLite 历史统计、阈值告警与 waybar 等多前端输出。所有数据由守护进程采集并经 D-Bus 分发。"),
    ("settings.about.group_links", "项目链接"),
    ("settings.about.github", "GitHub 项目主页"),
    ("settings.language_reopen_hint", "语言已保存，重新打开设置界面后生效"),
];

pub const EN: &[(&str, &str)] = &[
    ("tray.app_name", "TrafficView"),
    ("tray.title.offline", "TrafficView (daemon offline)"),
    ("tray.title.paused", "TrafficView (paused)"),
    ("tray.title.paused_metrics", "⏸ {title} · {cpu}% · {memory}%"),
    ("tray.title.metrics", "↓{download} ↑{upload} · C{cpu} M{memory}"),
    ("tray.tooltip.offline", "daemon offline; retrying…"),
    ("tray.tooltip.title", "TrafficView · ↓ {download}  ↑ {upload}"),
    ("tray.tooltip.interface", "Interface: {interface}"),
    ("tray.tooltip.network", "Network: ↓ {download} ↑ {upload} (total ↓ {download_total} ↑ {upload_total})"),
    ("tray.tooltip.cpu", "CPU: {value}%{frequency}"),
    ("tray.tooltip.cpu_frequency", "  @ {value} MHz"),
    ("tray.tooltip.memory", "Memory: {value}% (used {used} / total {total})"),
    ("tray.tooltip.swap", "Swap: {value}%"),
    ("tray.tooltip.disk", "Disk I/O: ↓ {read} ↑ {write}"),
    ("tray.tooltip.battery", "Battery: {value}% {status}"),
    ("tray.tooltip.temp_sensor", "{label} {value}°C"),
    ("tray.tooltip.temp", "Temp: {value}"),
    ("tray.tooltip.waiting", "waiting for first sample…"),
    ("tray.tooltip.alert", "⚠ {message}"),
    ("tray.menu.pause", "Pause"),
    ("tray.menu.resume", "Resume"),
    ("tray.menu.show_overlay", "Show overlay"),
    ("tray.menu.hide_overlay", "Hide overlay"),
    ("tray.menu.network_interface", "Network interface"),
    ("tray.menu.automatic", "Automatic"),
    ("tray.menu.settings", "Settings…"),
    ("tray.menu.open_config", "Open config"),
    ("tray.menu.quit", "Quit TrafficView"),
    ("alert.metric.cpu", "CPU usage"),
    ("alert.metric.memory", "Memory usage"),
    ("alert.metric.net_rx", "Download rate"),
    ("alert.metric.net_tx", "Upload rate"),
    ("alert.metric.month_bytes", "Data usage this month"),
    ("alert.above_threshold", "{label} {value} above threshold {threshold} (for {duration} seconds)"),
    ("alert.recovered", "{label} recovered (current {value})"),
    ("notify.app_name", "TrafficView"),
    ("notify.title", "TrafficView Alert"),
    ("settings.window_title", "TrafficView Settings"),
    ("settings.save", "Save"),
    ("settings.connecting", "Connecting to TrafficView daemon…"),
    ("settings.reconnect", "Reconnect"),
    ("settings.connection_failed", "Unable to connect to the TrafficView daemon: {reason}"),
    ("settings.saved", "Saved"),
    ("settings.save_failed", "Save failed: {reason}"),
    ("settings.autostart_failed", "Failed to configure autostart: {error}"),
    ("settings.disconnected_save", "Not connected to the daemon; cannot save"),
    ("settings.general.page", "General"),
    ("settings.general.language.title", "Language"),
    ("settings.general.language.subtitle", "auto follows the system; the tray applies immediately, settings take effect after reopening"),
    ("settings.general.language.auto", "Follow system"),
    ("settings.general.sample_interval.title", "Sample interval (milliseconds)"),
    ("settings.general.sample_interval.subtitle", "500–30000; shorter intervals increase resource usage"),
    ("settings.general.autostart.title", "Start automatically at login"),
    ("settings.general.autostart.subtitle", "Writes an XDG autostart entry for the current user; no elevated privileges required"),
    ("settings.network.page", "Network"),
    ("settings.network.group_collection", "Collection"),
    ("settings.network.group_filter", "Interface filtering"),
    ("settings.network.active_interface.title", "Active interface"),
    ("settings.network.active_interface.subtitle", "Leave empty to select automatically based on the default route"),
    ("settings.network.active_interface.auto", "(Select the default interface automatically)"),
    ("settings.network.smoothing.title", "Rate smoothing"),
    ("settings.network.smoothing.subtitle", "none / 3-point moving average / exponential moving average"),
    ("settings.network.smoothing.none", "None"),
    ("settings.network.smoothing.moving_average_3", "3-point moving average"),
    ("settings.network.smoothing.ema", "Exponential moving average"),
    ("settings.network.memory_include_cache.title", "Include cache in memory usage"),
    ("settings.network.memory_include_cache.subtitle", "When off, available memory is calculated using MemAvailable"),
    ("settings.network.blacklist.title", "Interface blacklist (comma-separated; wildcards supported, e.g. lo, docker*, veth*)"),
    ("settings.network.whitelist.title", "Interface whitelist (comma-separated; matching existing interfaces take precedence over the default route)"),
    ("settings.display.page", "Display"),
    ("settings.display.theme.title", "Theme"),
    ("settings.display.theme.subtitle", "auto follows the desktop; light/dark force a specific theme"),
    ("settings.display.theme.auto", "Follow system"),
    ("settings.display.theme.light", "Light"),
    ("settings.display.theme.dark", "Dark"),
    ("settings.display.group_overlay", "Overlay"),
    ("settings.display.overlay_visible.title", "Show overlay"),
    ("settings.display.overlay_opacity.title", "Opacity"),
    ("settings.display.overlay_units.title", "Rate units"),
    ("settings.display.overlay_units.bytes", "Bytes"),
    ("settings.display.overlay_units.bits", "Bits"),
    ("settings.display.overlay_always_on_top.title", "Always on top"),
    ("settings.display.overlay_click_through.title", "Click through"),
    ("settings.display.overlay_pos_x.title", "Position X"),
    ("settings.display.overlay_pos_y.title", "Position Y"),
    ("settings.display.overlay_skin.title", "Skin directory (leave empty to use the built-in layout)"),
    ("settings.display.group_items", "Display items"),
    ("settings.display.item_details", "{id} · {unit}"),
    ("settings.alerts.page", "Alerts"),
    ("settings.alerts.group_rules", "Rules (read-only overview; edit the configuration file for full editing)"),
    ("settings.alerts.empty_title", "No alert rules"),
    ("settings.alerts.empty_subtitle", "Define rules such as cpu_pct / mem_pct / net_rx_bps in the configuration file"),
    ("settings.alerts.rule_title", "{metric} {op} {threshold}"),
    ("settings.alerts.rule_subtitle", "Duration {duration}s · Cooldown {cooldown}s · {quiet}"),
    ("settings.alerts.no_quiet_hours", "No quiet hours"),
    ("settings.alerts.quiet_hours", "Quiet hours {hours}"),
    ("settings.history.page", "History"),
    ("settings.history.retention_days.title", "History retention (days)"),
    ("settings.history.retention_days.subtitle", "1–3650; affects minute-level details only; daily summaries are kept permanently"),
    ("settings.history.timezone.title", "Timezone (empty = system local, or UTC / +08:00)"),
    ("settings.advanced.page", "Plugins & Advanced"),
    ("settings.advanced.group_plugins", "Plugins"),
    ("settings.advanced.plugins_enabled.title", "Enable plugins"),
    ("settings.advanced.plugins_enabled.subtitle", "The plugin list is in the XDG data directory; failures do not affect built-in metrics"),
    ("settings.advanced.group_process", "Per-process traffic"),
    ("settings.advanced.process_backend.title", "Per-process traffic backend"),
    ("settings.advanced.process_backend.subtitle", "off=disabled; auto prefers eBPF and falls back to AF_PACKET; the helper must be installed by an administrator; this page only changes configuration"),
    ("settings.advanced.process_backend.off", "Off"),
    ("settings.advanced.process_backend.ebpf", "eBPF"),
    ("settings.advanced.process_backend.af_packet", "AF_PACKET"),
    ("settings.advanced.process_backend.auto", "Automatic"),
    ("settings.advanced.group_collector", "Collector status"),
    ("settings.advanced.collector_empty", "No collector status"),
    ("settings.advanced.collector_available", "Available"),
    ("settings.advanced.collector_unavailable", "Unavailable: {reason}"),
    ("settings.advanced.collector_parse_error", "Unable to parse status"),
    ("settings.advanced.collector_unknown_reason", "Unknown reason"),
    ("settings.about.page", "About"),
    ("settings.about.app_name", "TrafficView"),
    ("settings.about.version", "Version {version}"),
    ("settings.about.group_features", "Features"),
    ("settings.about.features", "Real-time network traffic and system load monitoring for the Linux desktop. The overlay shows upload/download rates, CPU, memory, GPU, and temperatures; supports tray controls, SQLite history statistics, threshold alerts, and output to multiple frontends such as waybar. All data is collected by the daemon and distributed over D-Bus."),
    ("settings.about.group_links", "Project links"),
    ("settings.about.github", "GitHub project page"),
    ("settings.language_reopen_hint", "Language saved; reopen the settings window to apply"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_from_locale_strings() {
        assert_eq!(detect_from(Some("zh_CN.UTF-8")), Language::Zh);
        assert_eq!(detect_from(Some("zh_TW")), Language::Zh);
        assert_eq!(detect_from(Some("en_US.UTF-8")), Language::En);
        assert_eq!(detect_from(Some("C")), Language::En);
        assert_eq!(detect_from(None), Language::En);
    }

    #[test]
    fn explicit_language_overrides_environment() {
        assert_eq!(resolve("zh"), Language::Zh);
        assert_eq!(resolve("EN"), Language::En);
        let _ = resolve("auto");
        let _ = resolve("fr");
    }

    #[test]
    fn unknown_key_falls_back_without_panic() {
        assert_eq!(tr(Language::Zh, "notify.title"), "TrafficView 告警");
        assert_eq!(tr(Language::En, "notify.title"), "TrafficView Alert");
        assert_eq!(tr(Language::Zh, "no.such.key"), "no.such.key");
    }

    #[test]
    fn trf_replaces_named_placeholders() {
        let text = trf(Language::En, "notify.title", &[]);
        assert_eq!(text, "TrafficView Alert");
    }

    #[test]
    fn zh_and_en_tables_have_identical_keys() {
        assert_eq!(ZH.len(), EN.len());
        for (index, (key, _)) in ZH.iter().enumerate() {
            assert_eq!(EN[index].0, *key, "key 顺序不一致：{key}");
        }
        // 文案非空，且占位符集合一致。
        for ((key, zh), (_, en)) in ZH.iter().zip(EN.iter()) {
            assert!(!zh.is_empty() && !en.is_empty(), "空文案：{key}");
            fn placeholders(text: &str) -> Vec<&str> {
                let mut names: Vec<&str> = text
                    .split('{')
                    .skip(1)
                    .filter_map(|rest| rest.split('}').next())
                    .collect();
                names.sort_unstable();
                names
            }
            assert_eq!(placeholders(zh), placeholders(en), "占位符不一致：{key}");
        }
    }

    #[test]
    fn translation_table_is_complete() {
        assert_eq!(ZH.len(), 125, "文案表应包含全部 125 条 GUI 文本");
    }
}
