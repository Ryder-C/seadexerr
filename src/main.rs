mod anilist;
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

use crate::anilist::AniListClient;
use crate::config::AppConfig;
use crate::mapping::PlexAniBridgeMappings;
use crate::releases::ReleasesClient;
use crate::sonarr::SonarrClient;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub anilist: AniListClient,
    pub sonarr: SonarrClient,
    pub releases: ReleasesClient,
    pub mappings: PlexAniBridgeMappings,
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

    let anilist = AniListClient::new(config.anilist_base_url.clone(), config.anilist_timeout)
        .context("failed to construct AniList client")?;

    let sonarr_cache_path = config.data_path.join("sonarr_titles.json");
    let sonarr = SonarrClient::new(
        config.sonarr_url.clone(),
        config.sonarr_api_key.clone(),
        config.sonarr_timeout,
        sonarr_cache_path,
    )
    .context("failed to construct Sonarr client")?;

    let mappings = PlexAniBridgeMappings::bootstrap(
        config.data_path.clone(),
        config.mapping_source_url.clone(),
        config.mapping_refresh_interval,
        config.mapping_timeout,
    )
    .await
    .context("failed to initialise PlexAniBridge mappings store")?;

    let state = Arc::new(AppState {
        config,
        anilist,
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
