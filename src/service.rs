use std::collections::HashMap;

use crate::anilist::{AniListClient, AniListError, MediaFormat};
use crate::config::AppConfig;
use crate::mapping::{PlexAniBridgeMappings, TvdbMapping, parse_season_key};
use crate::radarr::{RadarrClient, RadarrError};
use crate::releases::{ReleasesClient, ReleasesError, Torrent, Tracker};
use crate::sonarr::{SonarrClient, SonarrError};
use crate::torznab::{self, ANIME_CATEGORY, MOVIE_CATEGORY, TorznabItem};
use thiserror::Error;
use tracing::{debug, info, trace, warn};

/// Upper bound on items returned in a single torznab response.
const MAX_RESPONSE_ITEMS: usize = 100;

/// Seeder value to use for a torrent based on its tracker and preference.
/// A value needs to be 10x greater than the next lower value for Sonarr/Radarr to prioritize it.
fn priority_seeders(tracker: Tracker, preferred: bool) -> u32 {
    match (tracker, preferred) {
        (Tracker::AnimeBytes, true) => 10000,
        (Tracker::Nyaa, true) => 1000,
        (Tracker::AnimeBytes, false) => 100,
        (Tracker::Nyaa, false) => 10,
    }
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("{0:#}")]
    Mapping(anyhow::Error),
    #[error(transparent)]
    Releases(#[from] ReleasesError),
    #[error(transparent)]
    AniList(#[from] AniListError),
    #[error(transparent)]
    Sonarr(#[from] SonarrError),
    #[error(transparent)]
    Radarr(#[from] RadarrError),
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
            .unwrap_or(MAX_RESPONSE_ITEMS)
            .clamp(1, MAX_RESPONSE_ITEMS);
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
                info!(
                    tvdb_id,
                    season, "no anilist mapping found; returning empty result set"
                );
                return Ok((Vec::new(), 0));
            }
        };

        trace!(tvdb_id, season, anilist_id, "querying releases.moe");

        let collected: Vec<Torrent> = self
            .releases
            .search_torrents(anilist_id)
            .await
            .map_err(ServiceError::Releases)?;

        if collected.is_empty() {
            info!(
                tvdb_id,
                season, anilist_id, "no releases found on releases.moe; returning empty result set"
            );
            return Ok((Vec::new(), 0));
        }

        let media_lookup = self
            .anilist
            .fetch_media(&[anilist_id])
            .await
            .map_err(ServiceError::AniList)?;

        let Some(media) = media_lookup.get(&anilist_id) else {
            debug!(
                tvdb_id,
                season, anilist_id, "AniList media missing; returning empty result set"
            );
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

        let torrents: Vec<Torrent> = collected
            .into_iter()
            .filter(|item| item.files.len() > 1)
            .filter(|item| !self.is_excluded(item))
            .collect();
        let total = torrents.len();

        let feed_title = match self.resolve_feed_title(tvdb_id, season).await {
            Ok(title) => title,
            Err(ServiceError::Sonarr(SonarrError::Api { .. } | SonarrError::NotFound { .. })) => {
                debug!(
                    tvdb_id,
                    season, "Sonarr series lookup failed; returning empty result set"
                );
                return Ok((Vec::new(), 0));
            }
            Err(err) => return Err(err),
        };

        let items = self.process_torrents_ranked(
            torrents,
            feed_title,
            self.tv_category_ids(),
            offset,
            limit,
        );
        Ok((items, total))
    }

    pub async fn search_movie(
        &self,
        tmdb_id: i64,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<(Vec<TorznabItem>, usize), ServiceError> {
        let limit = limit
            .unwrap_or(MAX_RESPONSE_ITEMS)
            .clamp(1, MAX_RESPONSE_ITEMS);
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
                info!(
                    tmdb_id,
                    "no anilist mapping found for movie-search; returning empty result set"
                );
                return Ok((Vec::new(), 0));
            }
        };

        trace!(
            tmdb_id,
            anilist_id, limit, "movie-search querying releases.moe"
        );

        let collected: Vec<Torrent> = self
            .releases
            .search_torrents(anilist_id)
            .await
            .map_err(ServiceError::Releases)?;

        if collected.is_empty() {
            info!(
                tmdb_id,
                anilist_id, "no releases found on releases.moe; returning empty result set"
            );
            return Ok((Vec::new(), 0));
        }

        let media_lookup = self
            .anilist
            .fetch_media(&[anilist_id])
            .await
            .map_err(ServiceError::AniList)?;

        let Some(media) = media_lookup.get(&anilist_id) else {
            debug!(
                tmdb_id,
                anilist_id, "AniList media missing for movie-search; returning empty result set"
            );
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

        let torrents: Vec<Torrent> = collected
            .into_iter()
            .filter(|item| !self.is_excluded(item))
            .collect();
        let total = torrents.len();

        let feed_title = match self.radarr.as_ref().unwrap().resolve_name(tmdb_id).await {
            Ok(movie) => self.format_movie_feed_title(&movie.title, movie.year),
            Err(RadarrError::NotFound { .. } | RadarrError::Api { .. }) => {
                debug!(
                    tmdb_id,
                    "Radarr movie lookup failed; returning empty result set"
                );
                return Ok((Vec::new(), 0));
            }
            Err(err) => return Err(ServiceError::Radarr(err)),
        };

        let items = self.process_torrents_ranked(
            torrents,
            feed_title,
            self.movie_category_ids(),
            offset,
            limit,
        );

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
            .unwrap_or(MAX_RESPONSE_ITEMS)
            .clamp(1, MAX_RESPONSE_ITEMS);
        let offset = offset.unwrap_or(0);

        if query.is_some() {
            debug!(
                limit,
                offset, "generic search query unsupported; returning empty results"
            );
            return Ok((Vec::new(), 0));
        }

        if !self.category_filter_matches(&cat) {
            debug!(
                limit,
                offset, "category filter unsupported; returning empty results"
            );
            return Ok((Vec::new(), 0));
        }

        trace!(limit, offset, "serving search via recent public torrents");

        let mut torrents = self
            .releases
            .recent_public_torrents()
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

            if include && !self.is_excluded(&torrent) {
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
                    let title = match self
                        .resolve_tv_generic_title(&torrent, &mut tv_title_cache)
                        .await
                    {
                        Ok(Some(title)) => title,
                        Ok(None) => {
                            debug!(torrent_id = %torrent.id, "no tv title resolved for generic search; skipping");
                            continue;
                        }
                        Err(error) => {
                            warn!(
                                torrent_id = %torrent.id,
                                %error,
                                "failed to resolve tv title for generic search; skipping"
                            );
                            continue;
                        }
                    };
                    grouped_torrents
                        .entry((title, self.tv_category_ids()))
                        .or_default()
                        .push(torrent);
                }
                MediaFormat::Movie if self.radarr.is_some() => {
                    match self
                        .resolve_movie_generic_title(anilist_id, &mut movie_title_cache)
                        .await
                    {
                        Ok(Some(title)) => {
                            grouped_torrents
                                .entry((title, self.movie_category_ids()))
                                .or_default()
                                .push(torrent);
                        }
                        Ok(None) => {
                            debug!(torrent_id = %torrent.id, "no movie title resolved for generic search; skipping");
                        }
                        Err(error) => {
                            warn!(
                                torrent_id = %torrent.id,
                                %error,
                                "failed to resolve movie title for generic search; skipping"
                            );
                        }
                    }
                }
                _ => {}
            }
        }

        for ((title, categories), torrents) in grouped_torrents {
            items.extend(self.process_torrents(torrents, title, categories));
        }

        Ok((items, total))
    }

    pub async fn resolve_tv_generic_title(
        &self,
        torrent: &Torrent,
        cache: &mut HashMap<(i64, u32), String>,
    ) -> Result<Option<String>, ServiceError> {
        let Some(anilist_id) = torrent.anilist_id else {
            return Ok(None);
        };

        let mappings = self
            .mappings
            .resolve_tvdb_mappings(anilist_id)
            .await
            .map_err(ServiceError::Mapping)?;

        if mappings.is_empty() {
            return Ok(None);
        }

        if let Some((tvdb_id, season)) = self.select_tvdb_and_season(&mappings) {
            if let Some(existing) = cache.get(&(tvdb_id, season)) {
                return Ok(Some(existing.clone()));
            }

            let title = self.resolve_feed_title(tvdb_id, season).await?;
            cache.insert((tvdb_id, season), title.clone());
            return Ok(Some(title));
        }

        Ok(None)
    }

    pub async fn resolve_movie_generic_title(
        &self,
        anilist_id: i64,
        cache: &mut HashMap<i64, String>,
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
            return Ok(Some(existing.clone()));
        }

        let radarr = self
            .radarr
            .as_ref()
            .expect("resolve_movie_generic_title requires Radarr to be enabled");

        let movie = match radarr.resolve_name(tmdb_id).await {
            Ok(movie) => movie,
            Err(RadarrError::NotFound { .. } | RadarrError::Api { .. }) => return Ok(None),
            Err(err) => return Err(ServiceError::Radarr(err)),
        };

        let formatted = self.format_movie_feed_title(&movie.title, movie.year);
        cache.insert(tmdb_id, formatted.clone());
        Ok(Some(formatted))
    }

    pub async fn resolve_feed_title(
        &self,
        tvdb_id: i64,
        season: u32,
    ) -> Result<String, ServiceError> {
        trace!(tvdb_id, season, "resolving title from sonarr");
        let sonarr = self
            .sonarr
            .as_ref()
            .expect("resolve_feed_title requires Sonarr to be enabled");
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
        torrents
            .into_iter()
            .map(|release| {
                let seeders = priority_seeders(release.tracker, release.is_best);
                self.build_torznab_item(release, title.clone(), categories.clone(), seeders)
            })
            .collect()
    }

    /// Ranks the full torrent set, then returns the `offset`/`limit` page. Scoring
    /// must see every candidate so the preferred pick is stable across pages.
    fn process_torrents_ranked(
        &self,
        torrents: Vec<Torrent>,
        title: String,
        categories: Vec<u32>,
        offset: usize,
        limit: usize,
    ) -> Vec<TorznabItem> {
        let priorities = self.config.scoring.priorities(&torrents);

        torrents
            .into_iter()
            .zip(priorities)
            .skip(offset)
            .take(limit)
            .map(|(release, preferred)| {
                let seeders = priority_seeders(release.tracker, preferred);
                self.build_torznab_item(release, title.clone(), categories.clone(), seeders)
            })
            .collect()
    }

    fn is_excluded(&self, release: &Torrent) -> bool {
        let excluded = self.config.scoring.is_excluded(&release.tags);
        if excluded {
            trace!(torrent_id = %release.id, "excluding torrent due to excluded tag");
        }
        excluded
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
            ..
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
                            category.id == id
                                || category.subcategories.iter().any(|sub| sub.id == id)
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
