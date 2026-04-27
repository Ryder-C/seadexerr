use std::collections::{HashMap, HashSet};

use crate::anilist::{AniListClient, AniListError, MediaFormat};
use crate::config::{self, AppConfig};
use crate::mapping::{MappingError, PlexAniBridgeMappings, TvdbMapping, parse_season_key};
use crate::radarr::{RadarrClient, RadarrError};
use crate::releases::{ReleasesClient, ReleasesError, Torrent};
use crate::sonarr::{SonarrClient, SonarrError};
use crate::torznab::{self, TorznabItem, ANIME_CATEGORY, MOVIE_CATEGORY};
use thiserror::Error;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Mapping(#[from] MappingError),
    #[error(transparent)]
    Releases(#[from] ReleasesError),
    #[error(transparent)]
    AniList(#[from] AniListError),
    #[error(transparent)]
    Sonarr(#[from] SonarrError),
    #[error(transparent)]
    Radarr(#[from] RadarrError),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

pub struct SearchService {
    pub anilist: AniListClient,
    pub sonarr: Option<SonarrClient>,
    pub radarr: Option<RadarrClient>,
    pub releases: ReleasesClient,
    pub mappings: PlexAniBridgeMappings,
    pub config: AppConfig,
}

impl SearchService {
    pub fn new(
        anilist: AniListClient,
        sonarr: Option<SonarrClient>,
        radarr: Option<RadarrClient>,
        releases: ReleasesClient,
        mappings: PlexAniBridgeMappings,
        config: AppConfig,
    ) -> Self {
        Self {
            anilist,
            sonarr,
            radarr,
            releases,
            mappings,
            config,
        }
    }

    pub async fn search_tv(
        &self,
        tvdb_id: i64,
        season: u32,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<(Vec<TorznabItem>, usize), ServiceError> {
        let limit = limit
            .unwrap_or(config::DEFAULT_LIMIT)
            .clamp(1, config::DEFAULT_LIMIT);
        let offset = offset.unwrap_or(0);

        if self.sonarr.is_none() {
            debug!("tvsearch requested but sonarr is disabled; returning empty results");
            return Ok((Vec::new(), 0));
        }

        trace!(tvdb_id, season, "resolving plexanibridge mapping");

        let anilist_id = match self
            .mappings
            .resolve_anilist_id(tvdb_id, season)
            .await
            .map_err(ServiceError::Mapping)?
        {
            Some(id) => id,
            None => {
                info!(tvdb_id, season, "no anilist mapping found; returning empty result set");
                return Ok((Vec::new(), 0));
            }
        };

        trace!(tvdb_id, season, anilist_id, "querying releases.moe");

        let fetch_limit = offset.saturating_add(limit).min(config::DEFAULT_LIMIT);
        let collected: Vec<Torrent> = self
            .releases
            .search_torrents(anilist_id, fetch_limit)
            .await
            .map_err(ServiceError::Releases)?;

        if collected.is_empty() {
            info!(tvdb_id, season, anilist_id, "no releases found on releases.moe; returning empty result set");
            return Ok((Vec::new(), 0));
        }

        let media_lookup = self
            .anilist
            .fetch_media(&[anilist_id])
            .await
            .map_err(ServiceError::AniList)?;

        let Some(media) = media_lookup.get(&anilist_id) else {
            debug!(tvdb_id, season, anilist_id, "AniList media missing; returning empty result set");
            return Ok((Vec::new(), 0));
        };

        if !self.format_allowed(&media.format) {
            debug!(
                tvdb_id,
                season,
                anilist_id,
                format = ?media.format,
                "AniList format currently unsupported; returning empty result set"
            );
            return Ok((Vec::new(), 0));
        }

        let total = collected.len();
        let feed_title = match self.resolve_feed_title(tvdb_id, season).await {
            Ok(title) => title,
            Err(ServiceError::Sonarr(SonarrError::Api { .. } | SonarrError::NotFound { .. })) => {
                debug!(tvdb_id, season, "Sonarr series lookup failed; returning empty result set");
                return Ok((Vec::new(), 0));
            }
            Err(err) => return Err(err),
        };

        let torrents: Vec<Torrent> = collected
            .into_iter()
            .filter(|item| item.files.len() > 1)
            .skip(offset)
            .take(limit)
            .collect();

        let items = self.process_torrents(torrents, feed_title, self.tv_category_ids());
        Ok((items, total))
    }

    pub async fn search_movie(
        &self,
        tmdb_id: i64,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<(Vec<TorznabItem>, usize), ServiceError> {
        let limit = limit
            .unwrap_or(config::DEFAULT_LIMIT)
            .clamp(1, config::DEFAULT_LIMIT);
        let offset = offset.unwrap_or(0);

        if self.radarr.is_none() {
            debug!("movie-search requested but radarr is disabled; returning empty results");
            return Ok((Vec::new(), 0));
        }

        let anilist_id = match self
            .mappings
            .resolve_anilist_id_for_tmdb(tmdb_id)
            .await
            .map_err(ServiceError::Mapping)?
        {
            Some(id) => id,
            None => {
                info!(tmdb_id, "no anilist mapping found for movie-search; returning empty result set");
                return Ok((Vec::new(), 0));
            }
        };

        trace!(tmdb_id, anilist_id, limit, "movie-search querying releases.moe");

        let fetch_limit = offset.saturating_add(limit).min(config::DEFAULT_LIMIT);
        let collected: Vec<Torrent> = self
            .releases
            .search_torrents(anilist_id, fetch_limit)
            .await
            .map_err(ServiceError::Releases)?;

        if collected.is_empty() {
            info!(tmdb_id, anilist_id, "no releases found on releases.moe; returning empty result set");
            return Ok((Vec::new(), 0));
        }

        let media_lookup = self
            .anilist
            .fetch_media(&[anilist_id])
            .await
            .map_err(ServiceError::AniList)?;

        let Some(media) = media_lookup.get(&anilist_id) else {
            debug!(tmdb_id, anilist_id, "AniList media missing for movie-search; returning empty result set");
            return Ok((Vec::new(), 0));
        };

        if !self.movie_format_allowed(&media.format) {
            debug!(
                tmdb_id,
                anilist_id,
                format = ?media.format,
                "AniList format unsupported for movie-search"
            );
            return Ok((Vec::new(), 0));
        }

        let total = collected.len();
        let feed_title = match self
            .radarr
            .as_ref()
            .unwrap()
            .resolve_name(tmdb_id)
            .await
        {
            Ok(movie) => self.format_movie_feed_title(&movie.title, movie.year),
            Err(RadarrError::NotFound { .. } | RadarrError::Api { .. }) => {
                debug!(tmdb_id, "Radarr movie lookup failed; returning empty result set");
                return Ok((Vec::new(), 0));
            }
            Err(err) => return Err(ServiceError::Radarr(err)),
        };

        let torrents: Vec<Torrent> = collected.into_iter().skip(offset).take(limit).collect();
        let items = self.process_torrents(torrents, feed_title, self.movie_category_ids());

        Ok((items, total))
    }

    pub async fn search_generic(
        &self,
        query: Option<String>,
        cat: Option<String>,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<(Vec<TorznabItem>, usize), ServiceError> {
        let limit = limit
            .unwrap_or(config::DEFAULT_LIMIT)
            .clamp(1, config::DEFAULT_LIMIT);
        let offset = offset.unwrap_or(0);

        if query.is_some() {
            debug!(limit, offset, "generic search query unsupported; returning empty results");
            return Ok((Vec::new(), 0));
        }

        if !self.category_filter_matches(&cat) {
            debug!(limit, offset, "category filter unsupported; returning empty results");
            return Ok((Vec::new(), 0));
        }

        trace!(limit, offset, "serving search via recent public torrents");

        let fetch_limit = config::DEFAULT_LIMIT;
        let mut torrents = self
            .releases
            .recent_public_torrents(fetch_limit)
            .await
            .map_err(ServiceError::Releases)?;

        if torrents.is_empty() {
            return Ok((Vec::new(), 0));
        }

        let missing_ids: Vec<String> = torrents
            .iter()
            .filter(|torrent| torrent.anilist_id.is_none())
            .map(|torrent| torrent.id.clone())
            .collect();

        let resolved_anilist = if missing_ids.is_empty() {
            HashMap::new()
        } else {
            self.releases
                .resolve_anilist_ids_for_torrents(&missing_ids)
                .await
                .map_err(ServiceError::Releases)?
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

        let media_lookup = self
            .anilist
            .fetch_media(&anilist_ids)
            .await
            .map_err(ServiceError::AniList)?;

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
                format if self.format_allowed(format) => torrent.files.len() > 1,
                _ => false,
            };

            if include {
                eligible.push(torrent);
            }
        }

        let total = eligible.len();
        let window: Vec<Torrent> = eligible.into_iter().skip(offset).take(limit).collect();

        if window.is_empty() {
            return Ok((Vec::new(), total));
        }

        let mut tv_title_cache: HashMap<(i64, u32), String> = HashMap::new();
        let mut movie_title_cache: HashMap<i64, String> = HashMap::new();
        let mut active_tvdb_ids: HashSet<i64> = HashSet::new();
        let mut active_tmdb_ids: HashSet<i64> = HashSet::new();
        let mut items = Vec::with_capacity(window.len());

        let mut grouped_torrents: HashMap<(String, Vec<u32>), Vec<Torrent>> = HashMap::new();

        for torrent in window.into_iter() {
            let Some(anilist_id) = torrent.anilist_id else {
                continue;
            };

            let Some(media) = media_lookup.get(&anilist_id) else {
                continue;
            };

            match &media.format {
                format if self.format_allowed(format) && self.sonarr.is_some() => {
                    let title = match self.resolve_tv_generic_title(
                        &torrent,
                        &mut tv_title_cache,
                        &mut active_tvdb_ids,
                    )
                    .await
                    {
                        Ok(title) => title,
                        Err(error) => {
                            warn!(
                                torrent_id = %torrent.id,
                                %error,
                                "failed to resolve tv title for generic search; using fallback"
                            );
                            self.default_torrent_title(&torrent.id)
                        }
                    };
                    grouped_torrents
                        .entry((title, self.tv_category_ids()))
                        .or_default()
                        .push(torrent);
                }
                MediaFormat::Movie if self.radarr.is_some() => {
                    match self.resolve_movie_generic_title(
                        anilist_id,
                        &mut movie_title_cache,
                        &mut active_tmdb_ids,
                    )
                    .await
                    {
                        Ok(Some(title)) => {
                            grouped_torrents
                                .entry((title, self.movie_category_ids()))
                                .or_default()
                                .push(torrent);
                        }
                        Ok(None) => {
                            let fallback = self.default_torrent_title(&torrent.id);
                            grouped_torrents
                                .entry((fallback, self.movie_category_ids()))
                                .or_default()
                                .push(torrent);
                        }
                        Err(error) => {
                            warn!(
                                torrent_id = %torrent.id,
                                %error,
                                "failed to resolve movie title for generic search; using fallback"
                            );
                            let fallback = self.default_torrent_title(&torrent.id);
                            grouped_torrents
                                .entry((fallback, self.movie_category_ids()))
                                .or_default()
                                .push(torrent);
                        }
                    }
                }
                _ => {}
            }
        }

        for ((title, categories), torrents) in grouped_torrents {
            items.extend(self.process_torrents(torrents, title, categories));
        }

        if let Some(sonarr) = &self.sonarr {
            sonarr
                .retain_titles(&active_tvdb_ids)
                .await
                .map_err(ServiceError::Sonarr)?;
        }

        if let Some(radarr) = &self.radarr {
            radarr
                .retain_titles(&active_tmdb_ids)
                .await
                .map_err(ServiceError::Radarr)?;
        }

        Ok((items, total))
    }

    pub async fn resolve_tv_generic_title(
        &self,
        torrent: &Torrent,
        cache: &mut HashMap<(i64, u32), String>,
        active_tvdb_ids: &mut HashSet<i64>,
    ) -> Result<String, ServiceError> {
        let Some(anilist_id) = torrent.anilist_id else {
            return Ok(self.default_torrent_title(&torrent.id));
        };

        let mappings = self
            .mappings
            .resolve_tvdb_mappings(anilist_id)
            .await
            .map_err(ServiceError::Mapping)?;

        if mappings.is_empty() {
            return Ok(self.default_torrent_title(&torrent.id));
        }

        if let Some((tvdb_id, season)) = self.select_tvdb_and_season(&mappings) {
            active_tvdb_ids.insert(tvdb_id);

            if let Some(existing) = cache.get(&(tvdb_id, season)) {
                return Ok(existing.clone());
            }

            let title = self.resolve_feed_title(tvdb_id, season).await?;
            cache.insert((tvdb_id, season), title.clone());
            return Ok(title);
        }

        Ok(self.default_torrent_title(&torrent.id))
    }

    pub async fn resolve_movie_generic_title(
        &self,
        anilist_id: i64,
        cache: &mut HashMap<i64, String>,
        active_tmdb_ids: &mut HashSet<i64>,
    ) -> Result<Option<String>, ServiceError> {
        let Some(tmdb_id) = self
            .mappings
            .resolve_tmdb_id(anilist_id)
            .await
            .map_err(ServiceError::Mapping)?
        else {
            return Ok(None);
        };

        if let Some(existing) = cache.get(&tmdb_id) {
            active_tmdb_ids.insert(tmdb_id);
            return Ok(Some(existing.clone()));
        }

        let radarr = self
            .radarr
            .as_ref()
            .ok_or_else(|| ServiceError::Unsupported("Radarr is disabled".to_string()))?;

        let movie = match radarr.resolve_name(tmdb_id).await {
            Ok(movie) => movie,
            Err(RadarrError::NotFound { .. } | RadarrError::Api { .. }) => return Ok(None),
            Err(err) => return Err(ServiceError::Radarr(err)),
        };

        let formatted = self.format_movie_feed_title(&movie.title, movie.year);
        cache.insert(tmdb_id, formatted.clone());
        active_tmdb_ids.insert(tmdb_id);
        Ok(Some(formatted))
    }

    pub async fn resolve_feed_title(&self, tvdb_id: i64, season: u32) -> Result<String, ServiceError> {
        trace!(tvdb_id, season, "resolving title from sonarr");
        let sonarr = self
            .sonarr
            .as_ref()
            .ok_or_else(|| ServiceError::Unsupported("Sonarr is disabled".to_string()))?;
        let series_title = sonarr
            .resolve_name(tvdb_id)
            .await
            .map_err(ServiceError::Sonarr)?;
        trace!(tvdb_id, %series_title, "resolved series title from sonarr");
        Ok(format!("{series_title} S{season:02} Bluray 1080p remux"))
    }

    pub fn format_movie_feed_title(&self, title: &str, year: u32) -> String {
        if year == 0 {
            format!("{title} Bluray 1080p remux")
        } else {
            format!("{title} ({year}) Bluray 1080p remux")
        }
    }

    fn format_allowed(&self, format: &MediaFormat) -> bool {
        matches!(
            format,
            MediaFormat::Tv | MediaFormat::TvShort | MediaFormat::Ona
        )
    }

    fn movie_format_allowed(&self, format: &MediaFormat) -> bool {
        matches!(format, MediaFormat::Movie)
    }

    fn tv_category_ids(&self) -> Vec<u32> {
        let mut ids = vec![ANIME_CATEGORY.id];
        if let Some(sub) = ANIME_CATEGORY.subcategories.first() {
            ids.push(sub.id);
        }
        ids
    }

    fn movie_category_ids(&self) -> Vec<u32> {
        vec![MOVIE_CATEGORY.id]
    }

    fn process_torrents(
        &self,
        torrents: Vec<Torrent>,
        title: String,
        categories: Vec<u32>,
    ) -> Vec<TorznabItem> {
        let filtered: Vec<Torrent> = torrents
            .into_iter()
            .filter(|torrent| {
                if self.config.skip_deband && torrent.tags.contains(&"Deband Required".to_string()) {
                    trace!(torrent_id = %torrent.id, "skipping torrent due to Deband Required tag");
                    return false;
                }
                true
            })
            .collect();

        let has_dual_audio = self.config.prefer_dual_audio && filtered.iter().any(|t| t.dual_audio);

        filtered
            .into_iter()
            .map(|torrent| {
                let seeders = if self.config.prefer_dual_audio {
                    if has_dual_audio {
                        if torrent.dual_audio { 1000 } else { 100 }
                    } else if torrent.is_best {
                        1000
                    } else {
                        100
                    }
                } else if torrent.is_best {
                    1000
                } else {
                    100
                };

                self.build_torznab_item(torrent, title.clone(), categories.clone(), seeders)
            })
            .collect()
    }

    fn build_torznab_item(
        &self,
        torrent: Torrent,
        title: String,
        categories: Vec<u32>,
        seeders: u32,
    ) -> TorznabItem {
        let Torrent {
            id,
            download_url,
            source_url,
            info_hash,
            published,
            size_bytes,
            is_best: _,
            dual_audio: _,
            tags: _,
            files: _,
            anilist_id: _,
        } = torrent;

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

    fn select_tvdb_and_season(&self, mappings: &[TvdbMapping]) -> Option<(i64, u32)> {
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

    fn default_torrent_title(&self, id: &str) -> String {
        format!("Torrent {id}")
    }

    fn category_filter_matches(&self, cat_param: &Option<String>) -> bool {
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
                    if let Ok(id) = trimmed.parse::<u32>()
                        && categories.iter().any(|category| {
                            category.id == id || category.subcategories.iter().any(|sub| sub.id == id)
                        })
                    {
                        matches_supported = true;
                    }
                }

                if !any_values { true } else { matches_supported }
            }
        }
    }
}
