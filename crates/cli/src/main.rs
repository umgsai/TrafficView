use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};
use trafficview_collectors::{cpu::ProcCpu, mem::read_mem, net::ProcNetDev};
use trafficview_core::{clock::monotonic_ms, CpuSample, Snapshot};

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
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
        Commands::Sample {
            interface,
            interval_ms,
            count,
        } => {
            if count == 0 {
                bail!("--count must be greater than zero");
            }
            let started = Instant::now();
            let mut previous = started;
            let mut net = ProcNetDev::default();
            let mut cpu = ProcCpu::default();
            let _ = net
                .sample(&interface, Duration::ZERO)
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
                    .sample(&interface, elapsed)?
                    .context("network baseline was invalidated; retry")?;
                let (total_pct, per_core) = cpu
                    .sample(elapsed)?
                    .context("CPU baseline was invalidated; retry")?;
                let snapshot = Snapshot {
                    ts_ms: monotonic_ms(),
                    net,
                    cpu: CpuSample {
                        total_pct,
                        per_core,
                        freq_mhz: None,
                        load: None,
                    },
                    mem: read_mem(Path::new("/proc"))?,
                };
                println!("{}", serde_json::to_string(&snapshot)?);
            }
            Ok(())
        }
    }
}
