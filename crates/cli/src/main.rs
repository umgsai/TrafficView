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
    }
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
                    edited.validate().map_err(|message| anyhow::anyhow!(message))?;
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
    config.validate().map_err(|message| anyhow::anyhow!(message))?;
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
