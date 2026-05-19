mod anilist;
mod config;
mod http;
mod mapping;
mod radarr;
mod releases;
mod service;
mod sonarr;
mod torznab;

use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::anilist::AniListClient;
use crate::config::AppConfig;
use crate::mapping::PlexAniBridgeMappings;
use crate::radarr::RadarrClient;
use crate::releases::ReleasesClient;
use crate::service::SearchService;
use crate::sonarr::SonarrClient;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub anilist: AniListClient,
    pub sonarr: Option<SonarrClient>,
    pub radarr: Option<RadarrClient>,
    pub releases: ReleasesClient,
    pub mappings: PlexAniBridgeMappings,
    pub service: Arc<SearchService>,
}

pub type SharedAppState = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = AppConfig::from_env().context("failed to load configuration")?;
    let listen_addr = config.listen_addr;

    let releases = ReleasesClient::new().context("failed to construct releases.moe client")?;

    let anilist = AniListClient::new().context("failed to construct AniList client")?;

    let data_path = config::default_data_path();

    let mappings = PlexAniBridgeMappings::bootstrap(data_path.clone())
        .await
        .context("failed to initialise PlexAniBridge mappings store")?;

    let sonarr = if let Some(sonarr_config) = &config.sonarr {
        Some(
            SonarrClient::new(sonarr_config.clone(), data_path.clone())
                .context("failed to construct Sonarr client")?,
        )
    } else {
        None
    };

    let radarr = if let Some(radarr_config) = &config.radarr {
        Some(
            RadarrClient::new(radarr_config.clone(), data_path)
                .context("failed to construct Radarr client")?,
        )
    } else {
        None
    };

    let service = Arc::new(SearchService::new(
        anilist.clone(),
        sonarr.clone(),
        radarr.clone(),
        releases.clone(),
        mappings.clone(),
        config.clone(),
    ));

    let state = Arc::new(AppState {
        config,
        anilist,
        sonarr,
        radarr,
        releases,
        mappings,
        service,
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
