use trafficview_core::config::ConfigStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let store = ConfigStore::xdg().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    trafficview_daemon::service::run(store).await
}
