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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = AppConfig::from_env().context("failed to load configuration")?;
    let listen_addr = config.listen_addr;

    let http_client = http::client().context("failed to construct shared HTTP client")?;

    let releases = ReleasesClient::new(http_client.clone(), config.ab_passkey.as_deref())
        .context("failed to construct releases.moe client")?;

    let anilist =
        AniListClient::new(http_client.clone()).context("failed to construct AniList client")?;

    let data_path = config::default_data_path();

    let mappings = PlexAniBridgeMappings::bootstrap(data_path.clone())
        .await
        .context("failed to initialise PlexAniBridge mappings store")?;

    let sonarr = if let Some(sonarr_config) = &config.sonarr {
        Some(
            SonarrClient::new(
                http_client.clone(),
                sonarr_config.clone(),
                data_path.clone(),
            )
            .context("failed to construct Sonarr client")?,
        )
    } else {
        None
    };

    let radarr = if let Some(radarr_config) = &config.radarr {
        Some(
            RadarrClient::new(http_client, radarr_config.clone(), data_path)
                .context("failed to construct Radarr client")?,
        )
    } else {
        None
    };

    let state = Arc::new(SearchService::new(
        anilist, sonarr, radarr, releases, mappings, config,
    ));

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
