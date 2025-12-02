use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

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

use crate::anilist::{AniListError, MediaFormat};
use crate::radarr::RadarrError;
use crate::releases::{ReleasesError, Torrent};
use crate::torznab::{self, ChannelMetadata, TorznabItem};
use crate::{
    AppState, SharedAppState,
    mapping::{MappingError, TvdbMapping, parse_season_key},
    sonarr::SonarrError,
};

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

fn format_allowed(format: &MediaFormat) -> bool {
    matches!(
        format,
        MediaFormat::Tv | MediaFormat::TvShort | MediaFormat::Ona
    )
}

fn movie_format_allowed(format: &MediaFormat) -> bool {
    matches!(format, MediaFormat::Movie)
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
        TorznabOperation::MovieSearch => "movie-search",
        TorznabOperation::Unsupported(name) => name,
    };

    let valid = match &operation {
        TorznabOperation::Caps => true,
        TorznabOperation::Search => query.query.is_none() && category_filter_matches(&query.cat),
        TorznabOperation::TvSearch => {
            query.tvdb_identifier().is_some() && query.season_number().is_some()
        }
        TorznabOperation::MovieSearch => query.tmdb_identifier().is_some(),
        TorznabOperation::Unsupported(_) => false,
    };

    if valid {
        info!(
            operation = operation_name,
            tvdb = query.tvdb_id.as_deref(),
            tmdb = query.tmdb_id.as_deref(),
            season = query.season.as_deref(),
            limit = query.limit,
            "Valid torznab request received"
        );
    } else {
        debug!(
            operation = operation_name,
            tvdb = query.tvdb_id.as_deref(),
            tmdb = query.tmdb_id.as_deref(),
            season = query.season.as_deref(),
            limit = query.limit,
            "Invalid torznab request received"
        );
    }

    match operation {
        TorznabOperation::Caps => respond_caps(&state),
        TorznabOperation::Search => respond_generic_search(&state, &query).await,
        TorznabOperation::TvSearch => respond_tv_search(&state, &query).await,
        TorznabOperation::MovieSearch => respond_movie_search(&state, &query).await,
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

    let fetch_limit = state.config.default_limit;
    let mut torrents = state
        .releases
        .recent_public_torrents(fetch_limit)
        .await
        .map_err(HttpError::Releases)?;

    if torrents.is_empty() {
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    let missing_ids: Vec<String> = torrents
        .iter()
        .filter(|torrent| torrent.anilist_id.is_none())
        .map(|torrent| torrent.id.clone())
        .collect();

    let resolved_anilist = if missing_ids.is_empty() {
        HashMap::new()
    } else {
        state
            .releases
            .resolve_anilist_ids_for_torrents(&missing_ids)
            .await
            .map_err(HttpError::Releases)?
    };

    torrents = torrents
        .into_iter()
        .map(|mut torrent| {
            if torrent.anilist_id.is_none()
                && let Some(anilist_id) = resolved_anilist.get(&torrent.id).copied()
            {
                torrent.anilist_id = Some(anilist_id);
            }
            torrent
        })
        .collect();

    let anilist_ids: Vec<i64> = torrents
        .iter()
        .filter_map(|torrent| torrent.anilist_id)
        .collect();

    let media_lookup = state
        .anilist
        .fetch_media(&anilist_ids)
        .await
        .map_err(HttpError::AniList)?;

    let mut eligible: Vec<Torrent> = Vec::new();

    for torrent in torrents.into_iter() {
        let Some(anilist_id) = torrent.anilist_id else {
            continue;
        };

        let Some(media) = media_lookup.get(&anilist_id) else {
            continue;
        };

        let include = match &media.format {
            MediaFormat::Movie => true,
            format if format_allowed(format) => torrent.files.len() > 1,
            _ => false,
        };

        if include {
            eligible.push(torrent);
        }
    }

    let total = eligible.len();

    let window: Vec<Torrent> = eligible.into_iter().skip(offset).take(limit).collect();

    if window.is_empty() {
        let xml = torznab::render_feed(&metadata, &[], offset, total)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    let mut tv_title_cache: HashMap<(i64, u32), String> = HashMap::new();
    let mut movie_title_cache: HashMap<i64, String> = HashMap::new();
    let mut active_tvdb_ids: HashSet<i64> = HashSet::new();
    let mut active_tmdb_ids: HashSet<i64> = HashSet::new();
    let mut items = Vec::with_capacity(window.len());

    for torrent in window.into_iter() {
        let Some(anilist_id) = torrent.anilist_id else {
            debug!(torrent_id = %torrent.id, "skipping torrent without AniList id");
            continue;
        };

        let Some(media) = media_lookup.get(&anilist_id) else {
            debug!(
                anilist_id,
                "skipping torrent due to missing AniList metadata"
            );
            continue;
        };

        match &media.format {
            format if format_allowed(format) => {
                if state.sonarr.is_some() {
                    let title = resolve_tv_generic_title(
                        state,
                        &torrent,
                        &mut tv_title_cache,
                        &mut active_tvdb_ids,
                    )
                    .await?;
                    items.push(build_torznab_item(torrent, title, tv_category_ids()));
                }
            }
            MediaFormat::Movie => {
                if state.radarr.is_some() {
                    match resolve_movie_generic_title(
                        state,
                        anilist_id,
                        &mut movie_title_cache,
                        &mut active_tmdb_ids,
                    )
                    .await?
                    {
                        Some(title) => {
                            items.push(build_torznab_item(torrent, title, movie_category_ids()));
                        }
                        None => {
                            let fallback = default_torrent_title(&torrent.id);
                            items.push(build_torznab_item(torrent, fallback, movie_category_ids()));
                        }
                    }
                }
            }
            other => {
                debug!(
                    anilist_id,
                    format = ?other,
                    "skipping torrent due to unsupported AniList format"
                );
            }
        }
    }

    let xml = torznab::render_feed(&metadata, &items, offset, total)?;

    if let Some(sonarr) = &state.sonarr {
        sonarr
            .retain_titles(&active_tvdb_ids)
            .await
            .map_err(HttpError::Sonarr)?;
    }

    if let Some(radarr) = &state.radarr {
        radarr
            .retain_titles(&active_tmdb_ids)
            .await
            .map_err(HttpError::Radarr)?;
    }

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

    if state.sonarr.is_none() {
        debug!("tvsearch requested but sonarr is disabled; returning empty feed");
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

    let anilist_id = match state
        .mappings
        .resolve_anilist_id(tvdb_id, season)
        .await
        .map_err(HttpError::Mapping)?
    {
        Some(id) => id,
        None => {
            info!(
                tvdb_id,
                season, "no anilist mapping found; returning empty result set"
            );
            let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
            return Ok((
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response());
        }
    };

    debug!(tvdb_id, season, anilist_id, "querying releases.moe");

    let fetch_limit = offset.saturating_add(limit).min(state.config.default_limit);
    let collected: Vec<Torrent> = match state
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
    };

    let media_lookup = state
        .anilist
        .fetch_media(&[anilist_id])
        .await
        .map_err(HttpError::AniList)?;

    let Some(media) = media_lookup.get(&anilist_id) else {
        info!(
            tvdb_id,
            season, anilist_id, "AniList media missing; returning empty result set"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    };

    if !format_allowed(&media.format) {
        info!(
            tvdb_id,
            season,
            anilist_id,
            format = ?media.format,
            "AniList format currently unsupported; returning empty result set"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

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
        .map(|torrent| build_torznab_item(torrent, feed_title.clone(), tv_category_ids()))
        .collect();
    let xml = torznab::render_feed(&metadata, &items, offset, total)?;

    Ok((
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

async fn respond_movie_search(
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

    if state.radarr.is_none() {
        debug!("movie-search requested but radarr is disabled; returning empty feed");
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    let tmdb_id = match query.tmdb_identifier() {
        Some(id) => id,
        None => {
            debug!(
                limit,
                offset, "movie-search missing tmdbid; returning empty feed without error"
            );
            let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
            return Ok((
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response());
        }
    };

    let anilist_id = match state
        .mappings
        .resolve_anilist_id_for_tmdb(tmdb_id)
        .await
        .map_err(HttpError::Mapping)?
    {
        Some(id) => id,
        None => {
            info!(
                tmdb_id,
                "no anilist mapping found for movie-search; returning empty result set"
            );
            let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
            return Ok((
                [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
                xml,
            )
                .into_response());
        }
    };

    debug!(
        tmdb_id,
        anilist_id, limit, "movie-search querying releases.moe"
    );

    let fetch_limit = offset.saturating_add(limit).min(state.config.default_limit);
    let collected: Vec<Torrent> = match state
        .releases
        .search_torrents(anilist_id, fetch_limit)
        .await
    {
        Ok(torrents) => torrents,
        Err(err) => {
            tracing::error!(
                tmdb_id,
                anilist_id,
                error = %err,
                "releases.moe lookup failed for movie-search"
            );
            return Err(HttpError::Releases(err));
        }
    };

    let media_lookup = state
        .anilist
        .fetch_media(&[anilist_id])
        .await
        .map_err(HttpError::AniList)?;

    let Some(media) = media_lookup.get(&anilist_id) else {
        info!(
            tmdb_id,
            anilist_id, "AniList media missing for movie-search; returning empty result set"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    };

    if !movie_format_allowed(&media.format) {
        info!(
            tmdb_id,
            anilist_id,
            format = ?media.format,
            "AniList format unsupported for movie-search"
        );
        let xml = torznab::render_feed(&metadata, &[], offset, 0)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
            xml,
        )
            .into_response());
    }

    let total = collected.len();
    let feed_title = state
        .radarr
        .as_ref()
        .unwrap() // We can be sure Radarr is enabled here
        .resolve_name(tmdb_id)
        .await
        .map(|movie| format_movie_feed_title(&movie.title, movie.year))
        .map_err(HttpError::Radarr)?;
    let items: Vec<TorznabItem> = collected
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|torrent| build_torznab_item(torrent, feed_title.clone(), movie_category_ids()))
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
    let sonarr = state
        .sonarr
        .as_ref()
        .ok_or_else(|| HttpError::UnsupportedOperation("Sonarr is disabled".to_string()))?;
    let series_title = sonarr
        .resolve_name(tvdb_id)
        .await
        .map_err(HttpError::Sonarr)?;
    debug!(tvdb_id, %series_title, "resolved series title from sonarr");
    Ok(format!("{series_title} S{season:02} Bluray 1080p remux"))
}

fn format_movie_feed_title(title: &str, year: u32) -> String {
    if year == 0 {
        format!("{title} Bluray 1080p remux")
    } else {
        format!("{title} ({year}) Bluray 1080p remux")
    }
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

async fn resolve_tv_generic_title(
    state: &AppState,
    torrent: &crate::releases::Torrent,
    cache: &mut HashMap<(i64, u32), String>,
    active_tvdb_ids: &mut HashSet<i64>,
) -> Result<String, HttpError> {
    let Some(anilist_id) = torrent.anilist_id else {
        return Ok(default_torrent_title(&torrent.id));
    };

    let mappings = state
        .mappings
        .resolve_tvdb_mappings(anilist_id)
        .await
        .map_err(HttpError::Mapping)?;

    if mappings.is_empty() {
        return Ok(default_torrent_title(&torrent.id));
    }

    if let Some((tvdb_id, season)) = select_tvdb_and_season(&mappings) {
        active_tvdb_ids.insert(tvdb_id);

        if let Some(existing) = cache.get(&(tvdb_id, season)) {
            return Ok(existing.clone());
        }

        let title = resolve_feed_title(state, tvdb_id, season).await?;
        cache.insert((tvdb_id, season), title.clone());
        return Ok(title);
    }

    Ok(default_torrent_title(&torrent.id))
}

async fn resolve_movie_generic_title(
    state: &AppState,
    anilist_id: i64,
    cache: &mut HashMap<i64, String>,
    active_tmdb_ids: &mut HashSet<i64>,
) -> Result<Option<String>, HttpError> {
    let Some(tmdb_id) = state
        .mappings
        .resolve_tmdb_id(anilist_id)
        .await
        .map_err(HttpError::Mapping)?
    else {
        return Ok(None);
    };

    if let Some(existing) = cache.get(&tmdb_id) {
        active_tmdb_ids.insert(tmdb_id);
        return Ok(Some(existing.clone()));
    }

    let radarr = state
        .radarr
        .as_ref()
        .ok_or_else(|| HttpError::UnsupportedOperation("Radarr is disabled".to_string()))?;

    let movie = match radarr.resolve_name(tmdb_id).await {
        Ok(movie) => movie,
        Err(RadarrError::NotFound { .. }) => return Ok(None),
        Err(err) => return Err(HttpError::Radarr(err)),
    };

    let formatted = format_movie_feed_title(&movie.title, movie.year);
    cache.insert(tmdb_id, formatted.clone());
    active_tmdb_ids.insert(tmdb_id);
    Ok(Some(formatted))
}

fn select_tvdb_and_season(mappings: &[TvdbMapping]) -> Option<(i64, u32)> {
    let mut best: Option<(i64, u32)> = None;

    for mapping in mappings {
        let mut seasons: Vec<u32> = mapping
            .seasons
            .iter()
            .filter_map(|key| parse_season_key(key))
            .collect();

        if seasons.is_empty() {
            continue;
        }

        seasons.sort_unstable();
        let season = seasons[0];

        match best {
            Some((_, current)) if season >= current => {}
            _ => best = Some((mapping.tvdb_id, season)),
        }
    }

    best
}

fn default_torrent_title(id: &str) -> String {
    format!("Torrent {id}")
}

fn tv_category_ids() -> Vec<u32> {
    let mut ids = vec![torznab::ANIME_CATEGORY.id];
    if let Some(sub) = torznab::ANIME_CATEGORY.subcategories.first() {
        ids.push(sub.id);
    }
    ids
}

fn movie_category_ids() -> Vec<u32> {
    vec![torznab::MOVIE_CATEGORY.id]
}

fn build_torznab_item(
    torrent: crate::releases::Torrent,
    title: String,
    categories: Vec<u32>,
) -> TorznabItem {
    let crate::releases::Torrent {
        id,
        download_url,
        source_url,
        info_hash,
        published,
        size_bytes,
        is_best,
        files: _,
        anilist_id: _,
    } = torrent;

    let seeders = if is_best { 1000 } else { 100 };
    let comments = if source_url.is_empty() {
        None
    } else {
        Some(source_url)
    };

    TorznabItem {
        title,
        guid: id,
        link: download_url,
        comments,
        published,
        size_bytes,
        info_hash,
        seeders,
        leechers: 0,
        categories,
    }
}

fn category_filter_matches(cat_param: &Option<String>) -> bool {
    match cat_param {
        None => true,
        Some(value) => {
            let mut matches_supported = false;
            let mut any_values = false;
            let categories = torznab::default_categories();
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
                    if categories.iter().any(|category| {
                        category.id == id || category.subcategories.iter().any(|sub| sub.id == id)
                    }) {
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
    AniList(#[from] AniListError),
    #[error(transparent)]
    Sonarr(#[from] SonarrError),
    #[error(transparent)]
    Radarr(#[from] RadarrError),
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
            HttpError::AniList(_) => (
                StatusCode::BAD_GATEWAY,
                Cow::from("Failed to query AniList"),
            ),
            HttpError::Sonarr(SonarrError::Url(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct Sonarr request"),
            ),
            HttpError::Sonarr(_) => (StatusCode::BAD_GATEWAY, Cow::from("Failed to query Sonarr")),
            HttpError::Radarr(RadarrError::Url(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Cow::from("Failed to construct Radarr request"),
            ),
            HttpError::Radarr(_) => (StatusCode::BAD_GATEWAY, Cow::from("Failed to query Radarr")),
        };

        tracing::error!("torznab handler error: {self}");

        (status, message).into_response()
    }
}
