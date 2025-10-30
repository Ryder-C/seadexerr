use std::borrow::Cow;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tracing::{debug, info};
use url::Url;

use crate::releases::{ReleasesError, Torrent};
use crate::torznab::{self, ChannelMetadata, TorznabItem};
use crate::{AppState, SharedAppState};
use crate::{mapping::MappingError, sonarr::SonarrError};

pub fn router(state: SharedAppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api", get(torznab_handler))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
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
    #[serde(rename = "q")]
    query: Option<String>,
}

impl TorznabQuery {
    fn operation(&self) -> TorznabOperation<'_> {
        match self.operation.as_deref().unwrap_or("tvsearch") {
            "caps" => TorznabOperation::Caps,
            "search" => TorznabOperation::Search,
            "tvsearch" | "tv-search" => TorznabOperation::TvSearch,
            other => TorznabOperation::Unsupported(other),
        }
    }

    fn tvdb_identifier(&self) -> Option<i64> {
        self.tvdb_id
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
    Unsupported(&'a str),
}

async fn torznab_handler(
    State(state): State<SharedAppState>,
    Query(query): Query<TorznabQuery>,
) -> Result<Response, HttpError> {
    let operation = query.operation();
    let operation_name = match &operation {
        TorznabOperation::Caps => "caps",
        TorznabOperation::Search => "search",
        TorznabOperation::TvSearch => "tvsearch",
        TorznabOperation::Unsupported(name) => name,
    };

    info!(
        operation = operation_name,
        tvdb = query.tvdb_id.as_deref(),
        season = query.season.as_deref(),
        limit = query.limit,
        "torznab request received"
    );

    match operation {
        TorznabOperation::Caps => respond_caps(&state),
        TorznabOperation::Search => respond_generic_search(&state, &query).await,
        TorznabOperation::TvSearch => respond_tv_search(&state, &query).await,
        TorznabOperation::Unsupported(name) => {
            Err(HttpError::UnsupportedOperation(name.to_string()))
        }
    }
}

fn respond_caps(state: &AppState) -> Result<Response, HttpError> {
    let metadata = build_channel_metadata(state)?;
    let xml = torznab::render_caps(&metadata)?;
    Ok((
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

async fn respond_generic_search(
    state: &AppState,
    query: &TorznabQuery,
) -> Result<Response, HttpError> {
    let metadata = build_channel_metadata(state)?;
    let limit = query
        .limit
        .unwrap_or(state.config.default_limit)
        .max(1)
        .min(state.config.default_limit);
    let offset = query.offset.unwrap_or(0);

    if query.query.is_some() {
        debug!(
            limit,
            offset, "generic search query unsupported; returning empty feed"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    if !category_filter_matches(&query.cat) {
        debug!(
            limit,
            offset, "tvsearch category filter unsupported; returning empty feed"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    if !category_filter_matches(&query.cat) {
        debug!(
            limit,
            offset, "torznab search category filter unsupported; returning empty set"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    debug!(
        limit,
        offset, "serving torznab search via recent public torrents"
    );

    let fetch_limit = offset.saturating_add(limit).min(state.config.default_limit);
    let torrents = state
        .releases
        .recent_public_torrents(fetch_limit)
        .await
        .map_err(HttpError::Releases)?;

    let total = torrents.len();

    let items: Vec<TorznabItem> = torrents
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|torrent| map_torrent_search(torrent))
        .collect();
    let xml = torznab::render_feed(&metadata, &items, offset, total)?;

    Ok((
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

async fn respond_tv_search(state: &AppState, query: &TorznabQuery) -> Result<Response, HttpError> {
    let metadata = build_channel_metadata(state)?;
    let limit = query
        .limit
        .unwrap_or(state.config.default_limit)
        .max(1)
        .min(state.config.default_limit);

    let offset = query.offset.unwrap_or(0);

    let tvdb_id = match query.tvdb_identifier() {
        Some(id) => id,
        None => {
            debug!(
                limit,
                offset, "tvsearch missing tvdbid; returning empty feed without error"
            );
            let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
            return Ok((
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response());
        }
    };

    let season = match query.season_number() {
        Some(value) => value,
        None => {
            debug!(
                tvdb_id,
                limit, "tvsearch missing season; returning empty feed without error"
            );
            let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
            return Ok((
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response());
        }
    };

    debug!(tvdb_id, season, limit, "resolving plexanibridge mapping");

    let anilist_id = state
        .mappings
        .resolve_anilist_id(tvdb_id, season)
        .await
        .map_err(HttpError::Mapping)?;

    debug!(tvdb_id, season, ?anilist_id, "mapping lookup completed");

    let fetch_limit = offset.saturating_add(limit).min(state.config.default_limit);

    let collected: Vec<Torrent> = if let Some(anilist_id) = anilist_id {
        debug!(tvdb_id, season, anilist_id, "querying releases.moe");
        match state
            .releases
            .search_torrents(anilist_id, fetch_limit)
            .await
        {
            Ok(torrents) => torrents,
            Err(err) => {
                tracing::error!(
                    tvdb_id,
                    season,
                    anilist_id,
                    error = %err,
                    "releases.moe lookup failed"
                );
                return Err(HttpError::Releases(err));
            }
        }
    } else {
        info!(
            tvdb_id,
            season, "no anilist mapping found; returning empty result set"
        );
        Vec::new()
    };

    debug!(
        tvdb_id,
        season,
        matches = collected.len(),
        "prepared torznab feed items"
    );

    let total = collected.len();
    let feed_title = resolve_feed_title(state, tvdb_id, season).await?;

    let items: Vec<TorznabItem> = collected
        .into_iter()
        .filter(|item| item.files.len() > 1)
        .skip(offset)
        .take(limit)
        .enumerate()
        .map(|(i, torrent)| map_torrent_tvsearch(i, torrent, feed_title.clone()))
        .collect();
    let xml = torznab::render_feed(&metadata, &items, offset, total)?;

    Ok((
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

async fn resolve_feed_title(
    state: &AppState,
    tvdb_id: i64,
    season: u32,
) -> Result<String, HttpError> {
    debug!(tvdb_id, season, "resolving title from sonarr");
    let series_title = state
        .sonarr
        .resolve_name(tvdb_id)
        .await
        .map_err(HttpError::Sonarr)?;
    debug!(tvdb_id, %series_title, "resolved series title from sonarr");
    Ok(format!("{series_title} S{season:02} Bluray 1080p remux"))
}

fn build_channel_metadata(state: &AppState) -> Result<ChannelMetadata, HttpError> {
    let base = match state.config.public_base_url.clone() {
        Some(url) => url,
        None => Url::parse(&format!("http://{}", state.config.listen_addr))
            .map_err(|err| HttpError::BaseUrl(err.to_string()))?,
    };

    let site_link = base.clone();
    Ok(ChannelMetadata {
        title: state.config.application_title.clone(),
        description: state.config.application_description.clone(),
        site_link: site_link.to_string(),
    })
}

fn map_torrent_search(torrent: crate::releases::Torrent) -> TorznabItem {
    let crate::releases::Torrent {
        id,
        download_url,
        info_hash,
        published,
        size_bytes,
        is_best,
        files: _,
    } = torrent;

    let title = format!("Torrent {}", id);
    let seeders = if is_best { 1000 } else { 100 };

    TorznabItem {
        title,
        guid: id,
        link: download_url,
        published,
        size_bytes,
        info_hash,
        seeders,
        leechers: 0,
    }
}

fn map_torrent_tvsearch(
    index: usize,
    torrent: crate::releases::Torrent,
    override_title: String,
) -> TorznabItem {
    let crate::releases::Torrent {
        id,
        download_url,
        info_hash,
        published,
        size_bytes,
        is_best,
        files: _,
    } = torrent;

    let seeders = if is_best { 1000 } else { 100 };

    TorznabItem {
        title: override_title,
        guid: id,
        link: download_url,
        published,
        size_bytes,
        info_hash,
        seeders,
        leechers: 0,
    }
}

fn category_filter_matches(cat_param: &Option<String>) -> bool {
    match cat_param {
        None => true,
        Some(value) => {
            let mut matches_supported = false;
            let mut any_values = false;
            for part in value.split(',') {
                let trimmed = part.trim();
                if trimmed.is_empty() {
                    continue;
                }
                any_values = true;
                if trimmed == "0" {
                    return true;
                }
                if let Ok(id) = trimmed.parse::<u32>() {
                    if id == torznab::ANIME_CATEGORY.id
                        || torznab::ANIME_CATEGORY
                            .subcategories
                            .iter()
                            .any(|sub| sub.id == id)
                    {
                        matches_supported = true;
                    }
                }
            }
            if !any_values { true } else { matches_supported }
        }
    }
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("unsupported torznab operation `{0}`")]
    UnsupportedOperation(String),
    #[error("failed to construct torznab metadata base url: {0}")]
    BaseUrl(String),
    #[error(transparent)]
    Mapping(#[from] MappingError),
    #[error(transparent)]
    Releases(#[from] ReleasesError),
    #[error(transparent)]
    Torznab(#[from] torznab::TorznabBuildError),
    #[error(transparent)]
    Sonarr(#[from] SonarrError),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, message): (StatusCode, Cow<'static, str>) = match &self {
            HttpError::UnsupportedOperation(_) => {
                (StatusCode::BAD_REQUEST, Cow::from(self.to_string()))
            }
            HttpError::BaseUrl(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct public facing URL for seadexer indexer"),
            ),
            HttpError::Mapping(_) => (
                StatusCode::BAD_GATEWAY,
                Cow::from("Failed to resolve PlexAniBridge mapping for the requested query"),
            ),
            HttpError::Releases(ReleasesError::Url(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct releases.moe request"),
            ),
            HttpError::Releases(_) => (
                StatusCode::BAD_GATEWAY,
                Cow::from("Failed to query releases.moe"),
            ),
            HttpError::Torznab(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to render torznab payload"),
            ),
            HttpError::Sonarr(SonarrError::Url(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct Sonarr request"),
            ),
            HttpError::Sonarr(_) => (StatusCode::BAD_GATEWAY, Cow::from("Failed to query Sonarr")),
        };

        tracing::error!("torznab handler error: {self}");

        (status, message).into_response()
    }
}
