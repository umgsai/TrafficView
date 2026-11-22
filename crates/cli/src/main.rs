use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    env,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use trafficview_cli::{
    format_disconnected, format_paused, format_polybar, format_text, format_tmux, format_waybar,
    StatusFormat,
};
use trafficview_collectors::{cpu::ProcCpu, mem::read_mem, net::ProcNetDev};
use trafficview_core::{
    clock::monotonic_ms,
    config::{Config, ConfigStore, RateUnit},
    CpuSample, Snapshot,
};
use trafficview_render::{skin::Skin, Renderer};

#[derive(Parser)]
#[command(name = "trafficview", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Sample {
        #[arg(long)]
        interface: String,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(500..=30_000))]
        interval_ms: u64,
        #[arg(long, default_value_t = 1)]
        count: usize,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// 查询 daemon 当前快照。
    Status {
        /// M1 兼容别名：等价于 `--format json`。
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: StatusFormat,
        /// daemon 不可达时输出显式的 disconnected 状态（仅 waybar）。
        #[arg(long)]
        allow_disconnected: bool,
    },
    /// 校验或离屏预览原生 skin.toml。
    Skin {
        #[command(subcommand)]
        command: SkinCommand,
    },
    /// 暂停采样（暂停期间 status 返回退出码 2）。
    Pause,
    /// 恢复采样。
    Resume,
    /// 请求 daemon 退出。
    Quit,
    /// 启动 X11 悬浮窗（只订阅 D-Bus，不读 /proc）。
    Overlay {
        #[arg(long, default_value = "auto")]
        session: String,
    },
    /// 启动 StatusNotifierItem 托盘。
    Tray {
        #[arg(long)]
        autostart: bool,
    },
    /// 管理 XDG autostart 条目。
    Autostart {
        #[command(subcommand)]
        command: AutostartCommand,
    },
}

#[derive(Subcommand)]
enum AutostartCommand {
    Enable,
    Disable,
    Status,
}

#[derive(Subcommand)]
enum SkinCommand {
    /// 解析并校验皮肤，输出规范化摘要 JSON。
    Validate { path: std::path::PathBuf },
    /// 用快照 JSON 离屏渲染 PNG，不需要图形会话。
    Preview {
        path: std::path::PathBuf,
        #[arg(long)]
        snapshot: std::path::PathBuf,
        #[arg(long)]
        output: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// 打印配置文件路径。
    Path,
    /// 打印当前配置的 TOML。
    Get,
    /// 修改白名单键并写回（校验失败时不落盘）。
    Set { key: String, value: String },
    /// 使用 $VISUAL / $EDITOR 编辑，退出后校验再保存。
    Edit,
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
        Commands::Sample {
            interface,
            interval_ms,
            count,
        } => sample(&interface, interval_ms, count),
        Commands::Config { command } => config(command),
        Commands::Status {
            json,
            format,
            allow_disconnected,
        } => status(json, format, allow_disconnected),
        Commands::Skin { command } => skin(command),
        Commands::Pause => call_daemon("Pause", &(true)),
        Commands::Resume => call_daemon("Pause", &(false)),
        Commands::Quit => call_daemon("Quit", &()),
        Commands::Overlay { session } => {
            let config = ConfigStore::xdg()?.load_or_create()?;
            overlay(session, config)
        }
        Commands::Tray { autostart: _ } => trafficview_tray::run(),
        Commands::Autostart { command } => autostart(command),
    }
}

fn autostart_path() -> Result<std::path::PathBuf> {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .context("neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(config_home.join("autostart").join("trafficview.desktop"))
}

fn autostart_desktop(executable: &std::path::Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=TrafficView\nComment=Network, CPU and memory monitor\nExec={} tray --autostart\nIcon=trafficview\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        executable.display()
    )
}

fn autostart(command: AutostartCommand) -> Result<()> {
    let path = autostart_path()?;
    match command {
        AutostartCommand::Enable => {
            let executable = env::current_exe().context("cannot locate the trafficview binary")?;
            let contents = autostart_desktop(&executable);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // 幂等：内容相同时不重复写盘。
            if std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str()) {
                std::fs::write(&path, contents)?;
            }
            println!("{}", path.display());
        }
        AutostartCommand::Disable => match std::fs::remove_file(&path) {
            Ok(()) => println!("disabled {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                println!("already disabled")
            }
            Err(error) => return Err(error.into()),
        },
        AutostartCommand::Status => {
            if path.is_file() {
                println!("enabled {}", path.display());
            } else {
                println!("disabled {}", path.display());
            }
        }
    }
    Ok(())
}

fn connect() -> Result<zbus::blocking::Connection> {
    zbus::blocking::Connection::session()
        .context("cannot connect to the session bus; is a graphical session with D-Bus available?")
}

fn overlay(session: String, config: Config) -> Result<()> {
    match session.as_str() {
        "x11" => trafficview_ui_x11::run(config),
        "wayland" => {
            let desktop = env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
            let backend = trafficview_ui_wayland::Backend::choose("wayland", &desktop, true);
            trafficview_ui_wayland::run_with_backend(config, backend, true)
        }
        "auto" => {
            let session_type = env::var("XDG_SESSION_TYPE").unwrap_or_default();
            let wayland = session_type.eq_ignore_ascii_case("wayland")
                || (session_type.is_empty() && env::var_os("WAYLAND_DISPLAY").is_some());
            if !wayland {
                return trafficview_ui_x11::run(config);
            }
            let desktop = env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
            let backend = trafficview_ui_wayland::Backend::choose("wayland", &desktop, true);
            trafficview_ui_wayland::run_with_backend(config, backend, true)
        }
        other => bail!("unsupported --session {other}; use auto, x11 or wayland"),
    }
}

fn status(json: bool, format: StatusFormat, allow_disconnected: bool) -> Result<()> {
    let format = if json { StatusFormat::Json } else { format };
    let connection = match connect() {
        Ok(connection) => connection,
        Err(error) => {
            if format == StatusFormat::Waybar && allow_disconnected {
                println!("{}", format_disconnected()?);
                return Ok(());
            }
            return Err(error);
        }
    };
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "com.trafficview.Daemon1",
        "/com/trafficview/Daemon1",
        "com.trafficview.Daemon1",
    )
    .context("cannot build the trafficviewd proxy")?;
    let reply = proxy
        .call_method("GetSnapshot", &())
        .context("cannot query trafficviewd; is the daemon running?");
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) if format!("{error:#}").contains("daemon is paused") => {
            if format == StatusFormat::Waybar && allow_disconnected {
                println!("{}", format_paused()?);
                return Ok(());
            }
            eprintln!("daemon is paused");
            std::process::exit(2);
        }
        Err(error) => {
            if format == StatusFormat::Waybar && allow_disconnected {
                println!("{}", format_disconnected()?);
                return Ok(());
            }
            return Err(error);
        }
    };
    let dict: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        reply.body().deserialize()?;
    let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&dict)?;
    let bits = rate_units() == RateUnit::Bits;
    match format {
        StatusFormat::Text => println!("{}", format_text(&snapshot, bits)),
        StatusFormat::Json => println!("{}", serde_json::to_string(&snapshot)?),
        StatusFormat::Waybar => println!("{}", format_waybar(&snapshot, bits)?),
        StatusFormat::Polybar => println!("{}", format_polybar(&snapshot, bits)),
        StatusFormat::Tmux => println!("{}", format_tmux(&snapshot, bits)),
    }
    Ok(())
}

fn rate_units() -> RateUnit {
    ConfigStore::xdg()
        .ok()
        .and_then(|store| store.load().ok())
        .map(|config| config.overlay.units)
        .unwrap_or_default()
}

fn skin(command: SkinCommand) -> Result<()> {
    match command {
        SkinCommand::Validate { path } => {
            let skin = Skin::load(&path)?;
            print_skin_summary(&skin)?;
        }
        SkinCommand::Preview {
            path,
            snapshot,
            output,
        } => {
            let skin = Skin::load(&path)?;
            let source = std::fs::read_to_string(&snapshot)
                .with_context(|| format!("cannot read snapshot {}", snapshot.display()))?;
            let snapshot: Snapshot = serde_json::from_str(&source)
                .with_context(|| format!("invalid snapshot JSON in {}", snapshot.display()))?;
            let mut renderer = Renderer::new(skin)?;
            let frame = renderer.render(&snapshot, 1.0)?;
            let png =
                trafficview_render::encode_png(&frame).map_err(|error| anyhow::anyhow!(error))?;
            std::fs::write(&output, png)
                .with_context(|| format!("cannot write preview {}", output.display()))?;
            print_skin_summary(renderer.skin())?;
        }
    }
    Ok(())
}

fn print_skin_summary(skin: &Skin) -> Result<()> {
    for warning in &skin.warnings {
        eprintln!("warning: {warning}");
    }
    let items: Vec<serde_json::Value> = skin
        .items
        .iter()
        .map(|item| {
            let align = match item.align {
                trafficview_render::skin::Align::Left => "left",
                trafficview_render::skin::Align::Right => "right",
                trafficview_render::skin::Align::Center => "center",
            };
            let thresholds: Vec<serde_json::Value> = item
                .thresholds
                .iter()
                .map(
                    |threshold| serde_json::json!({ "at": threshold.at, "color": threshold.color }),
                )
                .collect();
            serde_json::json!({
                "id": item.id,
                "rect": item.rect,
                "label": item.label,
                "format": item.format,
                "color": item.color,
                "align": align,
                "mono": item.mono,
                "font_size": item.font_size,
                "thresholds": thresholds,
            })
        })
        .collect();
    let summary = serde_json::json!({
        "meta": {
            "name": skin.meta.name,
            "author": skin.meta.author,
            "version": skin.meta.version,
        },
        "canvas": {
            "width": skin.canvas.width,
            "height": skin.canvas.height,
            "background": skin.canvas.background,
            "background_color": skin.canvas.background_color,
            "nine_patch": skin.canvas.nine_patch,
            "opacity": skin.canvas.opacity,
            "units": match skin.canvas.units {
                RateUnit::Bytes => "bytes",
                RateUnit::Bits => "bits",
            },
        },
        "font": {
            "family": skin.font.family,
            "size": skin.font.size,
            "color": skin.font.color,
        },
        "items": items,
        "warnings": skin.warnings,
    });
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

fn call_daemon<B>(method: &str, body: &B) -> Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let connection = connect()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "com.trafficview.Daemon1",
        "/com/trafficview/Daemon1",
        "com.trafficview.Daemon1",
    )
    .context("cannot build the trafficviewd proxy")?;
    proxy
        .call_method(method, body)
        .with_context(|| format!("cannot call {method}; is trafficviewd running?"))?;
    println!("{method} ok");
    Ok(())
}

fn config(command: ConfigCommand) -> Result<()> {
    let store = ConfigStore::xdg()?;
    match command {
        ConfigCommand::Path => {
            println!("{}", store.path().display());
            Ok(())
        }
        ConfigCommand::Get => {
            let config = store.load_or_create()?;
            print!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut config = store.load_or_create()?;
            apply_setting(&mut config, &key, &value)?;
            store.save(&config)?;
            reload_daemon();
            println!("{} = {}", key, value);
            Ok(())
        }
        ConfigCommand::Edit => {
            let mut config = store.load_or_create()?;
            let editor = env::var("VISUAL")
                .or_else(|_| env::var("EDITOR"))
                .context("neither $VISUAL nor $EDITOR is set")?;
            let tmp = store.path().with_extension("toml.edit");
            std::fs::write(&tmp, toml::to_string_pretty(&config)?)?;
            let status = Command::new(&editor).arg(&tmp).status();
            let result = match status {
                Ok(status) if status.success() => (|| -> Result<()> {
                    let text = std::fs::read_to_string(&tmp)?;
                    let edited: Config = toml::from_str(&text)?;
                    edited
                        .validate()
                        .map_err(|message| anyhow::anyhow!(message))?;
                    store.save(&edited)?;
                    config = edited;
                    reload_daemon();
                    Ok(())
                })(),
                Ok(status) => Err(anyhow::anyhow!("editor exited with {status}")),
                Err(error) => Err(anyhow::anyhow!("cannot run {editor}: {error}")),
            };
            let _ = std::fs::remove_file(&tmp);
            result?;
            println!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
    }
}

fn reload_daemon() {
    // daemon 未运行时配置写入仍然成功，只是热加载稍后自然生效。
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &connection,
        "com.trafficview.Daemon1",
        "/com/trafficview/Daemon1",
        "com.trafficview.Daemon1",
    ) else {
        return;
    };
    let _ = proxy.call_method("ReloadConfig", &());
}

fn apply_setting(config: &mut Config, key: &str, value: &str) -> Result<()> {
    match key {
        "sample_interval_ms" => {
            config.sample_interval_ms = value
                .parse()
                .with_context(|| format!("{value:?} is not a positive integer"))?;
        }
        "active_interface" => {
            if value.chars().any(char::is_whitespace) {
                bail!("interface name must not contain whitespace");
            }
            config.active_interface = value.to_owned();
        }
        "memory_include_cache" => {
            config.memory_include_cache = value
                .parse()
                .with_context(|| format!("{value:?} is not a boolean"))?;
        }
        other => bail!("unsupported config key: {other}"),
    }
    config
        .validate()
        .map_err(|message| anyhow::anyhow!(message))?;
    Ok(())
}

fn sample(interface: &str, interval_ms: u64, count: usize) -> Result<()> {
    if count == 0 {
        bail!("--count must be greater than zero");
    }
    let started = Instant::now();
    let mut previous = started;
    let mut net = ProcNetDev::default();
    let mut cpu = ProcCpu::default();
    let _ = net
        .sample(interface, Duration::ZERO)
        .context("establish network baseline")?;
    let _ = cpu
        .sample(Duration::ZERO)
        .context("establish CPU baseline")?;
    for _ in 0..count {
        thread::sleep(Duration::from_millis(interval_ms));
        let now = Instant::now();
        let elapsed = now.duration_since(previous);
        previous = now;
        let net = net
            .sample(interface, elapsed)?
            .context("network baseline was invalidated; retry")?;
        let (total_pct, per_core) = cpu
            .sample(elapsed)?
            .context("CPU baseline was invalidated; retry")?;
        let snapshot = Snapshot::new(
            monotonic_ms(),
            net,
            CpuSample {
                total_pct,
                per_core,
                freq_mhz: None,
                load: None,
            },
            read_mem(Path::new("/proc"))?,
        );
        println!("{}", serde_json::to_string(&snapshot)?);
    }
    Ok(())
}
