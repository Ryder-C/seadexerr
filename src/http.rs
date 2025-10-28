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
use url::Url;

use crate::releases::{ReleasesError, TorrentSearchRequest};
use crate::torznab::{self, ChannelMetadata, TorznabAttr, TorznabEnclosure, TorznabItem};
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
    imdbid: Option<String>,
    season: Option<String>,
    ep: Option<String>,
}

impl TorznabQuery {
    fn operation(&self) -> TorznabOperation<'_> {
        match self.operation.as_deref().unwrap_or("search") {
            "caps" => TorznabOperation::Caps,
            "search" => TorznabOperation::Search,
            "tvsearch" | "tv-search" => TorznabOperation::TvSearch,
            other => TorznabOperation::Unsupported(other),
        }
    }

    fn query(&self) -> Option<&str> {
        self.q.as_deref()
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
    match query.operation() {
        TorznabOperation::Caps => respond_caps(&state),
        TorznabOperation::Search | TorznabOperation::TvSearch => {
            respond_search(&state, &query).await
        }
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

async fn respond_search(state: &AppState, query: &TorznabQuery) -> Result<Response, HttpError> {
    let metadata = build_channel_metadata(state)?;
    let limit = query.limit.unwrap_or(state.config.default_limit);

    let q = query
        .query()
        .ok_or_else(|| HttpError::MissingRequiredParameter("q"))?;

    let request = TorrentSearchRequest {
        query: q.to_string(),
        limit: Some(limit),
        offset: query.offset,
    };

    let torrents = state.releases.search_torrents(request).await?;
    let items: Vec<TorznabItem> = torrents.into_iter().map(map_torrent).collect();
    let xml = torznab::render_feed(&metadata, &items)?;

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

    if let Some(size) = torrent.size_bytes {
        attributes.push(TorznabAttr {
            name: "size".to_string(),
            value: size.to_string(),
        });
    }

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

    if let Some(info_hash) = torrent.info_hash.as_deref() {
        attributes.push(TorznabAttr {
            name: "infohash".to_string(),
            value: info_hash.to_string(),
        });
    }

    if let Some(magnet) = torrent.magnet_uri.as_deref() {
        attributes.push(TorznabAttr {
            name: "magneturl".to_string(),
            value: magnet.to_string(),
        });
    }

    attributes.push(TorznabAttr {
        name: "downloadvolumefactor".to_string(),
        value: "0".to_string(),
    });
    attributes.push(TorznabAttr {
        name: "uploadvolumefactor".to_string(),
        value: "1".to_string(),
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
        size_bytes: torrent.size_bytes,
        categories: vec![],
        attributes,
        enclosure: Some(enclosure),
    }
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("unsupported torznab operation `{0}`")]
    UnsupportedOperation(String),
    #[error("missing required torznab parameter `{0}`")]
    MissingRequiredParameter(&'static str),
    #[error("failed to construct torznab metadata base url: {0}")]
    BaseUrl(String),
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
            HttpError::MissingRequiredParameter(_) => {
                (StatusCode::BAD_REQUEST, Cow::from(self.to_string()))
            }
            HttpError::BaseUrl(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct public facing URL for seadexer indexer"),
            ),
            HttpError::Releases(ReleasesError::NotImplemented) => (
                StatusCode::NOT_IMPLEMENTED,
                Cow::from("Releases.moe integration has not been implemented yet"),
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
