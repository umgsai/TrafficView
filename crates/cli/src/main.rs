use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    env,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use trafficview_collectors::{cpu::ProcCpu, mem::read_mem, net::ProcNetDev};
use trafficview_core::{
    clock::monotonic_ms,
    config::{Config, ConfigStore},
    CpuSample, Snapshot,
};

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
        #[arg(long)]
        json: bool,
    },
    /// 暂停采样（暂停期间 status 返回退出码 2）。
    Pause,
    /// 恢复采样。
    Resume,
    /// 请求 daemon 退出。
    Quit,
    /// 启动 X11 悬浮窗（只订阅 D-Bus，不读 /proc）。
    Overlay {
        #[arg(long, default_value = "x11")]
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
        Commands::Status { json } => status(json),
        Commands::Pause => call_daemon("Pause", &(true)),
        Commands::Resume => call_daemon("Pause", &(false)),
        Commands::Quit => call_daemon("Quit", &()),
        Commands::Overlay { session } => {
            if session != "x11" {
                bail!("only --session x11 is supported in M1");
            }
            let config = ConfigStore::xdg()?.load_or_create()?;
            trafficview_ui_x11::run(config)
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

fn status(json: bool) -> Result<()> {
    let connection = connect()?;
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
            eprintln!("daemon is paused");
            std::process::exit(2);
        }
        Err(error) => return Err(error),
    };
    let dict: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        reply.body().deserialize()?;
    let snapshot = trafficview_daemon::dbus::snapshot_from_dbus(&dict)?;
    if json {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        println!(
            "iface={} ↓{} ↑{} CPU {:.0}% MEM {:.0}% ts={}ms",
            snapshot.net.iface,
            trafficview_core::format::format_rate(snapshot.net.rx_bps),
            trafficview_core::format::format_rate(snapshot.net.tx_bps),
            snapshot.cpu.total_pct,
            snapshot.mem.used_pct,
            snapshot.ts_ms
        );
    }
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
