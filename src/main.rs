mod anilist;
mod config;
mod http;
mod mapping;
mod radarr;
mod releases;
mod sonarr;
mod torznab;

use std::sync::Arc;

use anyhow::Context;
use reqwest::Url;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::anilist::AniListClient;
use crate::config::{
    ANILIST_BASE_URL, AppConfig, DEFAULT_LIMIT, MAPPING_REFRESH_INTERVAL, MAPPING_SOURCE_URL,
    RELEASES_BASE_URL, TIMEOUT, get_data_path,
};
use crate::mapping::PlexAniBridgeMappings;
use crate::radarr::RadarrClient;
use crate::releases::ReleasesClient;
use crate::sonarr::SonarrClient;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub anilist: AniListClient,
    pub sonarr: Option<SonarrClient>,
    pub radarr: Option<RadarrClient>,
    pub releases: ReleasesClient,
    pub mappings: PlexAniBridgeMappings,
}

pub type SharedAppState = Arc<AppState>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = AppConfig::from_env().context("failed to load configuration")?;
    let listen_addr = config.listen_addr;

    let releases_url = Url::parse(RELEASES_BASE_URL).context("invalid RELEASES_BASE_URL")?;
    let releases = ReleasesClient::new(releases_url, TIMEOUT, DEFAULT_LIMIT)
        .context("failed to construct releases.moe client")?;

    let anilist_url = Url::parse(ANILIST_BASE_URL).context("invalid ANILIST_BASE_URL")?;
    let anilist = AniListClient::new(anilist_url, TIMEOUT)
        .context("failed to construct AniList client")?;

    let data_path = get_data_path();

    let sonarr = if let Some(sonarr_config) = &config.sonarr {
        let sonarr_cache_path = data_path.join("sonarr_titles.json");
        Some(
            SonarrClient::new(
                sonarr_config.url.clone(),
                sonarr_config.api_key.clone(),
                TIMEOUT,
                sonarr_cache_path,
            )
            .context("failed to construct Sonarr client")?,
        )
    } else {
        None
    };

    let radarr = if let Some(radarr_config) = &config.radarr {
        let radarr_cache_path = data_path.join("radarr_titles.json");
        Some(
            RadarrClient::new(
                radarr_config.url.clone(),
                radarr_config.api_key.clone(),
                TIMEOUT,
                radarr_cache_path,
            )
            .context("failed to construct Radarr client")?,
        )
    } else {
        None
    };

    let mapping_source_url = Url::parse(MAPPING_SOURCE_URL).context("invalid MAPPING_SOURCE_URL")?;
    let mappings = PlexAniBridgeMappings::bootstrap(
        data_path,
        mapping_source_url,
        MAPPING_REFRESH_INTERVAL,
        TIMEOUT,
    )
    .await
    .context("failed to initialise PlexAniBridge mappings store")?;

    let state = Arc::new(AppState {
        config,
        anilist,
        sonarr,
        radarr,
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
