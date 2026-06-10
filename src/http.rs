use std::{borrow::Cow, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tracing::{debug, info};

use crate::service::{SearchService, ServiceError};
use crate::torznab::{self, ChannelMetadata, TorznabItem};

/// Timeout applied to every outbound HTTP client
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the shared HTTP client used by the AniList, releases.moe, Sonarr, and
/// Radarr clients. `reqwest::Client` is `Arc`-backed, so callers clone this once
/// and share the underlying connection pool, DNS resolver, and TLS config.
///
/// The mappings client is intentionally excluded: it needs distinct connect and
/// read timeouts (a longer read window for the ~9 MB asset) rather than the total
/// [`TIMEOUT`] this client applies.
pub fn client() -> reqwest::Result<Client> {
    Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Timeouts for the mappings download client. The shared [`TIMEOUT`] is a total
/// request timeout, too tight for the large (~9 MB) mappings asset served via a
/// redirect to GitHub's CDN. Instead we bound connect and per-read stalls, which
/// lets a healthy-but-slow download run to completion.
pub const MAPPINGS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAPPINGS_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub fn router(state: Arc<SearchService>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api", get(torznab_handler))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

pub struct TorznabResponse {
    pub metadata: ChannelMetadata,
    pub items: Vec<TorznabItem>,
    pub offset: usize,
    pub total: usize,
    pub is_caps: bool,
}

impl TorznabResponse {
    pub fn new(
        metadata: ChannelMetadata,
        items: Vec<TorznabItem>,
        offset: usize,
        total: usize,
    ) -> Self {
        Self {
            metadata,
            items,
            offset,
            total,
            is_caps: false,
        }
    }

    pub fn caps(metadata: ChannelMetadata) -> Self {
        Self {
            metadata,
            items: Vec::new(),
            offset: 0,
            total: 0,
            is_caps: true,
        }
    }

    pub fn empty(metadata: ChannelMetadata, offset: usize) -> Self {
        Self {
            metadata,
            items: Vec::new(),
            offset,
            total: 0,
            is_caps: false,
        }
    }
}

impl IntoResponse for TorznabResponse {
    fn into_response(self) -> Response {
        let result = if self.is_caps {
            torznab::render_caps(&self.metadata)
        } else {
            torznab::render_feed(&self.metadata, &self.items, self.offset, self.total)
        };

        match result {
            Ok(xml) => {
                let content_type = if self.is_caps {
                    "application/xml; charset=utf-8"
                } else {
                    "application/rss+xml; charset=utf-8"
                };
                ([(header::CONTENT_TYPE, content_type)], xml).into_response()
            }
            Err(err) => HttpError::Torznab(err).into_response(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct TorznabQuery {
    #[serde(rename = "t")]
    operation: Option<String>,
    cat: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    #[allow(dead_code)]
    imdbid: Option<String>,
    season: Option<String>,
    #[serde(rename = "tvdbid")]
    tvdb_id: Option<String>,
    #[serde(rename = "tmdbid")]
    tmdb_id: Option<String>,
    #[serde(rename = "q")]
    query: Option<String>,
}

impl TorznabQuery {
    fn operation(&self) -> TorznabOperation<'_> {
        match self.operation.as_deref().unwrap_or("tvsearch") {
            "caps" => TorznabOperation::Caps,
            "search" => TorznabOperation::Search,
            "tvsearch" | "tv-search" => TorznabOperation::TvSearch,
            "movie" | "movie-search" | "moviesearch" => TorznabOperation::MovieSearch,
            other => TorznabOperation::Unsupported(other),
        }
    }

    fn tvdb_identifier(&self) -> Option<i64> {
        self.tvdb_id
            .as_deref()
            .and_then(|value| value.trim().parse::<i64>().ok())
    }

    fn tmdb_identifier(&self) -> Option<i64> {
        self.tmdb_id
            .as_deref()
            .and_then(|value| value.trim().parse::<i64>().ok())
    }

    fn season_number(&self) -> Option<u32> {
        self.season
            .as_deref()
            .and_then(|value| value.trim().parse::<u32>().ok())
    }
}

enum TorznabOperation<'a> {
    Caps,
    Search,
    TvSearch,
    MovieSearch,
    Unsupported(&'a str),
}

async fn torznab_handler(
    State(state): State<Arc<SearchService>>,
    Query(query): Query<TorznabQuery>,
) -> Result<TorznabResponse, HttpError> {
    let operation = query.operation();
    let metadata = build_channel_metadata(&state)?;

    match operation {
        TorznabOperation::Caps => {
            info!("serving torznab capabilities");
            Ok(TorznabResponse::caps(metadata))
        }
        TorznabOperation::Search => {
            let (items, total) = state
                .search_generic(query.query, query.cat, query.limit, query.offset)
                .await?;
            Ok(TorznabResponse::new(
                metadata,
                items,
                query.offset.unwrap_or(0),
                total,
            ))
        }
        TorznabOperation::TvSearch => {
            let tvdb_id = query.tvdb_identifier();
            let season = query.season_number();

            if let (Some(tvdb_id), Some(season)) = (tvdb_id, season) {
                info!(tvdb_id, season, "handling tv search");
                let (items, total) = state
                    .search_tv(tvdb_id, season, query.limit, query.offset)
                    .await?;
                Ok(TorznabResponse::new(
                    metadata,
                    items,
                    query.offset.unwrap_or(0),
                    total,
                ))
            } else {
                debug!("tvsearch missing tvdbid or season; returning empty feed");
                Ok(TorznabResponse::empty(metadata, query.offset.unwrap_or(0)))
            }
        }
        TorznabOperation::MovieSearch => {
            if let Some(tmdb_id) = query.tmdb_identifier() {
                info!(tmdb_id, "handling movie search");
                let (items, total) = state
                    .search_movie(tmdb_id, query.limit, query.offset)
                    .await?;
                Ok(TorznabResponse::new(
                    metadata,
                    items,
                    query.offset.unwrap_or(0),
                    total,
                ))
            } else {
                debug!("movie-search missing tmdbid; returning empty feed");
                Ok(TorznabResponse::empty(metadata, query.offset.unwrap_or(0)))
            }
        }
        TorznabOperation::Unsupported(name) => {
            Err(HttpError::UnsupportedOperation(name.to_string()))
        }
    }
}

fn build_channel_metadata(state: &SearchService) -> Result<ChannelMetadata, HttpError> {
    let base = match state.config.public_base_url.clone() {
        Some(url) => url,
        None => url::Url::parse(&format!("http://{}", state.config.listen_addr))
            .map_err(|err| HttpError::BaseUrl(err.to_string()))?,
    };

    Ok(ChannelMetadata {
        title: crate::config::APPLICATION_TITLE.to_string(),
        description: crate::config::APPLICATION_DESCRIPTION.to_string(),
        site_link: base.to_string(),
    })
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("unsupported torznab operation `{0}`")]
    UnsupportedOperation(String),
    #[error("failed to construct torznab metadata base url: {0}")]
    BaseUrl(String),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Torznab(#[from] torznab::TorznabBuildError),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, message): (StatusCode, Cow<'static, str>) = match &self {
            HttpError::UnsupportedOperation(_) => {
                (StatusCode::BAD_REQUEST, Cow::from(self.to_string()))
            }
            HttpError::BaseUrl(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct public facing URL for seadexerr indexer"),
            ),
            HttpError::Service(err) => match err {
                ServiceError::Mapping(_) => (
                    StatusCode::BAD_GATEWAY,
                    Cow::from("Failed to resolve PlexAniBridge mapping for the requested query"),
                ),
                ServiceError::Releases(_) => (
                    StatusCode::BAD_GATEWAY,
                    Cow::from("Failed to query releases.moe"),
                ),
                ServiceError::AniList(_) => (
                    StatusCode::BAD_GATEWAY,
                    Cow::from("Failed to query AniList"),
                ),
                ServiceError::Sonarr(_) => {
                    (StatusCode::BAD_GATEWAY, Cow::from("Failed to query Sonarr"))
                }
                ServiceError::Radarr(_) => {
                    (StatusCode::BAD_GATEWAY, Cow::from("Failed to query Radarr"))
                }
            },
            HttpError::Torznab(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to render torznab payload"),
            ),
        };

        tracing::error!("torznab handler error: {self}");

        (status, message).into_response()
    }
}
