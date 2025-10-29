mod config;
mod http;
mod mapping;
mod releases;
mod sonarr;
mod torznab;

use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::AppConfig;
use crate::mapping::PlexAniBridgeClient;
use crate::releases::ReleasesClient;
use crate::sonarr::SonarrClient;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub sonarr: SonarrClient,
    pub releases: ReleasesClient,
    pub mappings: PlexAniBridgeClient,
}

pub type SharedAppState = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = AppConfig::from_env().context("failed to load configuration")?;
    let listen_addr = config.listen_addr;
    let releases = ReleasesClient::new(
        config.releases_base_url.clone(),
        config.releases_timeout,
        config.default_limit,
    )
    .context("failed to construct releases.moe client")?;

    let sonarr = SonarrClient::new(
        config.sonarr_url.clone(),
        config.sonarr_api_key.clone(),
        config.sonarr_timeout,
    )
    .context("failed to construct Sonarr client")?;

    let mappings = PlexAniBridgeClient::new(
        config.mapping_base_url.clone(),
        config.mapping_timeout,
        config.default_limit,
    )
    .context("failed to construct PlexAniBridge client")?;

    let state = Arc::new(AppState {
        config,
        sonarr,
        releases,
        mappings,
    });
    let app = http::router(state.clone());

    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("failed to bind listener on {listen_addr}"))?;

    tracing::info!(
        "listening for torznab requests on {}",
        listener.local_addr()?
    );

    axum::serve(listener, app.into_make_service())
        .await
        .context("server terminated unexpectedly")?;

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().without_time())
        .init();
}
