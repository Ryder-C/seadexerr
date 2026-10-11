use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::config::AppConfig;
use crate::ids::{AniListId, TmdbId, TvdbId};
use crate::mapping::{PlexAniBridgeMappings, TvdbMapping};
use crate::radarr::{RadarrClient, RadarrError};
use crate::releases::{ReleasesClient, ReleasesError, Torrent, Tracker};
use crate::sonarr::{SonarrClient, SonarrError};
use crate::torznab::{self, ANIME_CATEGORY, MOVIE_CATEGORY, TorznabItem};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, info, trace, warn};

/// Upper bound on items returned in a single torznab response.
const MAX_RESPONSE_ITEMS: usize = 100;

/// Cap on simultaneous Sonarr/Radarr title lookups during a generic search.
const MAX_CONCURRENT_TITLE_LOOKUPS: usize = 8;

/// The upstream lookup a torrent's feed title comes from during a generic search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TitleKey {
    Tv { tvdb_id: TvdbId, season: u32 },
    Movie { tmdb_id: TmdbId },
}

fn format_tv_feed_title(series_title: &str, season: u32) -> String {
    format!("{series_title} S{season:02}")
}

fn format_movie_feed_title(title: &str, year: u32) -> String {
    if year == 0 {
        title.to_string()
    } else {
        format!("{title} ({year})")
    }
}

/// Tags a feed title with the releases.moe record id, so a new SeaDex pick never
/// reuses the title of an already imported one (#59). Parentheses keep the
/// *arrs from parsing the id as a release group, as they would with brackets.
fn format_release_title(feed_title: &str, record_id: &str) -> String {
    format!("{feed_title} Bluray 1080p remux ({record_id})")
}

/// Picks the earliest regular season. Specials (season 0) are skipped because
/// they map to individual episodes, not a whole season pack.
fn select_tvdb_and_season(mappings: &[TvdbMapping]) -> Option<(TvdbId, u32)> {
    mappings
        .iter()
        .filter(|mapping| mapping.season > 0)
        .min_by_key(|mapping| mapping.season)
        .map(|mapping| (mapping.tvdb_id, mapping.season))
}

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
    Sonarr(#[from] SonarrError),
    #[error(transparent)]
    Radarr(#[from] RadarrError),
}

pub struct SearchService {
    pub sonarr: Option<SonarrClient>,
    pub radarr: Option<RadarrClient>,
    pub releases: ReleasesClient,
    pub mappings: PlexAniBridgeMappings,
    pub config: AppConfig,
}

impl SearchService {
    pub fn new(
        sonarr: Option<SonarrClient>,
        radarr: Option<RadarrClient>,
        releases: ReleasesClient,
        mappings: PlexAniBridgeMappings,
        config: AppConfig,
    ) -> Self {
        Self {
            sonarr,
            radarr,
            releases,
            mappings,
            config,
        }
    }

    pub async fn search_tv(
        &self,
        tvdb_id: TvdbId,
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

        if season == 0 {
            debug!(
                %tvdb_id,
                "specials can't be advertised as a season pack; returning empty results"
            );
            return Ok((Vec::new(), 0));
        }

        trace!(%tvdb_id, season, "resolving plexanibridge mapping");

        let anilist_id = match self
            .mappings
            .resolve_anilist_id(tvdb_id, season)
            .await
            .map_err(ServiceError::Mapping)?
        {
            Some(id) => id,
            None => {
                info!(
                    %tvdb_id,
                    season, "no anilist mapping found; returning empty result set"
                );
                return Ok((Vec::new(), 0));
            }
        };

        trace!(%tvdb_id, season, %anilist_id, "querying releases.moe");

        let collected: Vec<Torrent> = self
            .releases
            .search_torrents(anilist_id)
            .await
            .map_err(ServiceError::Releases)?;

        if collected.is_empty() {
            info!(
                %tvdb_id,
                season, %anilist_id, "no releases found on releases.moe; returning empty result set"
            );
            return Ok((Vec::new(), 0));
        }

        let torrents: Vec<Torrent> = collected
            .into_iter()
            .filter(|item| item.feature_count > 1)
            .filter(|item| !self.is_excluded(item))
            .collect();
        let total = torrents.len();

        let feed_title = match self.resolve_feed_title(tvdb_id, season).await {
            Ok(title) => title,
            Err(ServiceError::Sonarr(SonarrError::Api { .. } | SonarrError::NotFound { .. })) => {
                debug!(
                    %tvdb_id,
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
        tmdb_id: TmdbId,
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
                    %tmdb_id,
                    "no anilist mapping found for movie-search; returning empty result set"
                );
                return Ok((Vec::new(), 0));
            }
        };

        trace!(
            %tmdb_id,
            %anilist_id, limit, "movie-search querying releases.moe"
        );

        let collected: Vec<Torrent> = self
            .releases
            .search_torrents(anilist_id)
            .await
            .map_err(ServiceError::Releases)?;

        if collected.is_empty() {
            info!(
                %tmdb_id,
                %anilist_id, "no releases found on releases.moe; returning empty result set"
            );
            return Ok((Vec::new(), 0));
        }

        let torrents: Vec<Torrent> = collected
            .into_iter()
            .filter(|item| item.feature_count == 1)
            .filter(|item| !self.is_excluded(item))
            .collect();
        let total = torrents.len();

        let feed_title = match self.radarr.as_ref().unwrap().resolve_name(tmdb_id).await {
            Ok(movie) => format_movie_feed_title(&movie.title, movie.year),
            Err(RadarrError::NotFound { .. } | RadarrError::Api { .. }) => {
                debug!(
                    %tmdb_id,
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

        // Advertise each torrent under every title its AniList entry is mapped to,
        // so an entry mapped to both a TV season and a movie reaches both arrs.
        let mut targets: Vec<(Torrent, TitleKey)> = Vec::new();

        for torrent in torrents {
            let Some(anilist_id) = torrent.anilist_id else {
                continue;
            };

            if self.is_excluded(&torrent) {
                continue;
            }

            match self.title_keys(&torrent, anilist_id).await {
                Ok(keys) => targets.extend(keys.into_iter().map(|key| (torrent.clone(), key))),
                Err(error) => warn!(
                    torrent_id = %torrent.id,
                    %error,
                    "failed to resolve titles for generic search; skipping"
                ),
            }
        }

        let total = targets.len();
        let targets: Vec<(Torrent, TitleKey)> =
            targets.into_iter().skip(offset).take(limit).collect();

        if targets.is_empty() {
            return Ok((Vec::new(), total));
        }

        // Resolve each unique key against Sonarr/Radarr concurrently.
        let titles = self.resolve_titles_concurrently(&targets).await;

        // Group torrents under their resolved feed titles.
        let mut items = Vec::with_capacity(targets.len());
        let mut grouped_torrents: HashMap<(String, Vec<u32>), Vec<Torrent>> = HashMap::new();

        for (torrent, key) in targets {
            let Some(title) = titles.get(&key) else {
                continue;
            };
            let categories = match key {
                TitleKey::Tv { .. } => self.tv_category_ids(),
                TitleKey::Movie { .. } => self.movie_category_ids(),
            };
            grouped_torrents
                .entry((title.clone(), categories))
                .or_default()
                .push(torrent);
        }

        for ((title, categories), torrents) in grouped_torrents {
            items.extend(self.process_torrents(torrents, title, categories));
        }

        Ok((items, total))
    }

    /// Resolves the unique title keys in `targets` against Sonarr/Radarr, a
    /// bounded number at a time. Failed lookups are logged and omitted.
    async fn resolve_titles_concurrently(
        &self,
        targets: &[(Torrent, TitleKey)],
    ) -> HashMap<TitleKey, String> {
        let unique_keys: HashSet<TitleKey> = targets.iter().map(|(_, key)| *key).collect();
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_TITLE_LOOKUPS));
        let mut lookups: JoinSet<(TitleKey, Option<String>)> = JoinSet::new();

        for key in unique_keys {
            let semaphore = semaphore.clone();
            match key {
                TitleKey::Tv { tvdb_id, season } => {
                    let sonarr = self
                        .sonarr
                        .as_ref()
                        .expect("tv title lookups require Sonarr to be enabled")
                        .clone();
                    lookups.spawn(async move {
                        let _permit = semaphore
                            .acquire_owned()
                            .await
                            .expect("title lookup semaphore is never closed");
                        let title = match sonarr.resolve_name(tvdb_id).await {
                            Ok(series_title) => Some(format_tv_feed_title(&series_title, season)),
                            Err(error) => {
                                warn!(
                                    %tvdb_id,
                                    %error,
                                    "failed to resolve tv title for generic search; skipping"
                                );
                                None
                            }
                        };
                        (key, title)
                    });
                }
                TitleKey::Movie { tmdb_id } => {
                    let radarr = self
                        .radarr
                        .as_ref()
                        .expect("movie title lookups require Radarr to be enabled")
                        .clone();
                    lookups.spawn(async move {
                        let _permit = semaphore
                            .acquire_owned()
                            .await
                            .expect("title lookup semaphore is never closed");
                        let title = match radarr.resolve_name(tmdb_id).await {
                            Ok(movie) => Some(format_movie_feed_title(&movie.title, movie.year)),
                            Err(RadarrError::NotFound { .. } | RadarrError::Api { .. }) => {
                                debug!(
                                    %tmdb_id,
                                    "no movie title resolved for generic search; skipping"
                                );
                                None
                            }
                            Err(error) => {
                                warn!(
                                    %tmdb_id,
                                    %error,
                                    "failed to resolve movie title for generic search; skipping"
                                );
                                None
                            }
                        };
                        (key, title)
                    });
                }
            }
        }

        let mut titles = HashMap::new();
        while let Some(joined) = lookups.join_next().await {
            match joined {
                Ok((key, Some(title))) => {
                    titles.insert(key, title);
                }
                Ok((_, None)) => {}
                Err(error) => warn!(%error, "title lookup task failed"),
            }
        }
        titles
    }

    pub async fn resolve_feed_title(
        &self,
        tvdb_id: TvdbId,
        season: u32,
    ) -> Result<String, ServiceError> {
        trace!(%tvdb_id, season, "resolving title from sonarr");
        let sonarr = self
            .sonarr
            .as_ref()
            .expect("resolve_feed_title requires Sonarr to be enabled");
        let series_title = sonarr
            .resolve_name(tvdb_id)
            .await
            .map_err(ServiceError::Sonarr)?;
        trace!(%tvdb_id, %series_title, "resolved series title from sonarr");
        Ok(format_tv_feed_title(&series_title, season))
    }

    /// The Sonarr and Radarr titles a torrent can be advertised under. A single
    /// feature is a movie, several are a season pack.
    async fn title_keys(
        &self,
        torrent: &Torrent,
        anilist_id: AniListId,
    ) -> Result<Vec<TitleKey>, ServiceError> {
        let mut keys = Vec::new();

        if self.sonarr.is_some() && torrent.feature_count > 1 {
            let mappings = self
                .mappings
                .resolve_tvdb_mappings(anilist_id)
                .await
                .map_err(ServiceError::Mapping)?;
            if let Some((tvdb_id, season)) = select_tvdb_and_season(&mappings) {
                keys.push(TitleKey::Tv { tvdb_id, season });
            }
        }

        if self.radarr.is_some()
            && torrent.feature_count == 1
            && let Some(tmdb_id) = self
                .mappings
                .resolve_tmdb_id(anilist_id)
                .await
                .map_err(ServiceError::Mapping)?
        {
            keys.push(TitleKey::Movie { tmdb_id });
        }

        Ok(keys)
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
            title: format_release_title(&title, &id),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tvdb_season_selection_skips_specials() {
        let mapping = |tvdb_id, season| TvdbMapping {
            tvdb_id: TvdbId(tvdb_id),
            season,
        };

        assert_eq!(
            select_tvdb_and_season(&[mapping(1, 0), mapping(1, 3), mapping(2, 2)]),
            Some((TvdbId(2), 2))
        );
        assert_eq!(select_tvdb_and_season(&[mapping(1, 0)]), None);
    }

    #[test]
    fn release_title_includes_record_id() {
        assert_eq!(
            format_release_title(&format_tv_feed_title("Frieren", 1), "q1w2e3r4t5y6u7i"),
            "Frieren S01 Bluray 1080p remux (q1w2e3r4t5y6u7i)"
        );
        assert_eq!(
            format_release_title(&format_movie_feed_title("Akira", 1988), "q1w2e3r4t5y6u7i"),
            "Akira (1988) Bluray 1080p remux (q1w2e3r4t5y6u7i)"
        );
        assert_eq!(
            format_release_title(&format_movie_feed_title("Akira", 0), "q1w2e3r4t5y6u7i"),
            "Akira Bluray 1080p remux (q1w2e3r4t5y6u7i)"
        );
    }
}
