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

use crate::mapping::MappingError;
use crate::releases::{ReleasesError, Torrent};
use crate::torznab::{
    self, ChannelMetadata, TorznabAttr, TorznabCategoryRef, TorznabEnclosure, TorznabItem,
};
use crate::{AppState, SharedAppState};

pub fn router(state: SharedAppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/torznab/api", get(torznab_handler))
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
    q: Option<String>,
    cat: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    #[allow(dead_code)]
    imdbid: Option<String>,
    season: Option<String>,
    #[serde(rename = "tvdbid")]
    tvdb_id: Option<String>,
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

    if query
        .q
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        debug!(
            limit,
            offset, "torznab search received unsupported q parameter; returning empty set"
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
        .map(map_torrent)
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

    if query
        .q
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        debug!(
            limit,
            offset, "tvsearch received unsupported q parameter; returning empty feed"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

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

    let items: Vec<TorznabItem> = collected
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(map_torrent)
        .collect();
    let xml = torznab::render_feed(&metadata, &items, offset, total)?;

    Ok((
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

fn build_channel_metadata(state: &AppState) -> Result<ChannelMetadata, HttpError> {
    let base = match state.config.public_base_url.clone() {
        Some(url) => url,
        None => Url::parse(&format!("http://{}", state.config.listen_addr))
            .map_err(|err| HttpError::BaseUrl(err.to_string()))?,
    };

    let site_link = base.clone();
    let api_link = base
        .join("torznab/api")
        .map_err(|err| HttpError::BaseUrl(err.to_string()))?;

    Ok(ChannelMetadata {
        title: state.config.application_title.clone(),
        description: state.config.application_description.clone(),
        site_link: site_link.to_string(),
        api_link: api_link.to_string(),
    })
}

fn map_torrent(torrent: crate::releases::Torrent) -> TorznabItem {
    let mut attributes = Vec::new();

    let primary_category_id = torznab::ANIME_CATEGORY.id;
    let sub_category_id = torznab::ANIME_CATEGORY
        .subcategories
        .get(0)
        .map(|sub| sub.id)
        .unwrap_or(primary_category_id);

    if let Some(seeders) = torrent.seeders {
        attributes.push(TorznabAttr {
            name: "seeders".to_string(),
            value: seeders.to_string(),
        });
    }

    if let Some(leechers) = torrent.leechers {
        attributes.push(TorznabAttr {
            name: "peers".to_string(),
            value: (torrent.seeders.unwrap_or(0) as u64 + leechers as u64).to_string(),
        });
    }

    attributes.push(TorznabAttr {
        name: "category".to_string(),
        value: primary_category_id.to_string(),
    });
    attributes.push(TorznabAttr {
        name: "category".to_string(),
        value: sub_category_id.to_string(),
    });

    attributes.push(TorznabAttr {
        name: "type".to_string(),
        value: "series".to_string(),
    });

    let enclosure = TorznabEnclosure {
        url: torrent.download_url.clone(),
        length: torrent.size_bytes,
        mime_type: "application/x-bittorrent".to_string(),
    };

    TorznabItem {
        title: torrent.title,
        guid: torrent.id,
        guid_is_permalink: false,
        link: torrent.download_url,
        comments: torrent.comments,
        description: torrent.description,
        published: torrent.published,
        attributes,
        enclosure: Some(enclosure),
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
        };

        tracing::error!("torznab handler error: {self}");

        (status, message).into_response()
    }
}
