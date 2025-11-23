use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use reqwest::{
    Client, StatusCode,
    header::{ETAG, IF_NONE_MATCH},
};
use serde::Deserialize;
use thiserror::Error;
use tokio::fs;
use tokio::task;
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct PlexAniBridgeMappings {
    path: PathBuf,
    cache: Arc<RwLock<Option<CachedMappings>>>,
    client: Client,
    source_url: Url,
    refresh_interval: Duration,
}

#[derive(Debug)]
struct CachedMappings {
    modified: SystemTime,
    etag: Option<String>,
    entries: Arc<MappingIndex>,
}

#[derive(Debug, Clone)]
struct MappingEntry {
    anilist_id: i64,
    seasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct ReverseMappingEntry {
    tvdb_id: i64,
    seasons: Vec<String>,
}

#[derive(Debug)]
struct MappingIndex {
    tvdb_to_entries: HashMap<i64, Vec<MappingEntry>>,
    anilist_to_entries: HashMap<i64, Vec<ReverseMappingEntry>>,
    tmdb_to_anilist: HashMap<i64, i64>,
    anilist_to_tmdb: HashMap<i64, i64>,
}

#[derive(Debug, Clone)]
pub struct TvdbMapping {
    pub tvdb_id: i64,
    pub seasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawMappingRecord {
    #[serde(default)]
    tvdb_id: Option<i64>,
    #[serde(default)]
    tmdb_movie_id: Option<TmdbMovieId>,
    #[serde(default)]
    tvdb_mappings: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TmdbMovieId {
    Single(i64),
    Multiple(Vec<i64>),
}

impl TmdbMovieId {
    fn into_first(self) -> Option<i64> {
        match self {
            TmdbMovieId::Single(id) => Some(id),
            TmdbMovieId::Multiple(ids) => ids.into_iter().next(),
        }
    }
}

impl PlexAniBridgeMappings {
    pub async fn bootstrap(
        data_path: PathBuf,
        source_url: Url,
        refresh_interval: Duration,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        fs::create_dir_all(&data_path).await.with_context(|| {
            format!("failed to create data directory at {}", data_path.display())
        })?;

        let path = data_path.join("mappings.json");
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to construct PlexAniBridge HTTP client")?;

        let refresh_interval = if refresh_interval.is_zero() {
            Duration::from_secs(21_600)
        } else {
            refresh_interval
        };

        let mappings = Self {
            path,
            cache: Arc::new(RwLock::new(None)),
            client,
            source_url,
            refresh_interval,
        };

        mappings
            .refresh_mappings()
            .await
            .map_err(anyhow::Error::from)?;
        mappings.spawn_refresh_task();

        Ok(mappings)
    }

    fn spawn_refresh_task(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(this.refresh_interval).await;
                if let Err(error) = this.refresh_mappings().await {
                    warn!(
                        error = %error,
                        url = %this.source_url,
                        "failed to refresh plexanibridge mappings"
                    );
                }
            }
        });
    }

    async fn refresh_mappings(&self) -> Result<(), MappingError> {
        let etag_path = self.etag_path();
        let cached_etag = {
            let guard = self.cache.read().await;
            guard.as_ref().and_then(|cache| cache.etag.clone())
        };
        let cached_etag = if let Some(etag) = cached_etag {
            Some(etag)
        } else {
            match fs::read_to_string(&etag_path).await {
                Ok(value) => {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_owned())
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => {
                    warn!(
                        error = %error,
                        path = %etag_path.display(),
                        "failed to read cached etag; proceeding without conditional request"
                    );
                    None
                }
            }
        };

        let mut request = self.client.get(self.source_url.clone());
        if let Some(etag) = cached_etag {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request
            .send()
            .await
            .map_err(|source| MappingError::Download {
                source,
                url: self.source_url.clone(),
            })?;

        if response.status() == StatusCode::NOT_MODIFIED {
            debug!(
                path = %self.path.display(),
                url = %self.source_url,
                "plexanibridge mappings not modified; skipping refresh"
            );

            let cache_missing = {
                let guard = self.cache.read().await;
                guard.is_none()
            };

            if cache_missing {
                // ensure cache is hydrated so downstream calls can serve requests
                self.load_mappings().await?;
            }

            return Ok(());
        }

        let response = response
            .error_for_status()
            .map_err(|source| MappingError::Download {
                source,
                url: self.source_url.clone(),
            })?;

        let new_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_owned());

        let bytes = response
            .bytes()
            .await
            .map_err(|source| MappingError::Download {
                source,
                url: self.source_url.clone(),
            })?
            .to_vec();

        // Offload heavy JSON deserialisation and index build to a blocking thread so the
        // async runtime worker threads aren't stalled by CPU work.
        let index = {
            let bytes = bytes.clone();
            task::spawn_blocking(move || {
                let raw: HashMap<String, RawMappingRecord> = serde_json::from_slice(&bytes)?;
                Ok::<MappingIndex, MappingError>(Self::build_index(raw))
            })
            .await??
        };
        let series = index.tvdb_to_entries.len();
        let entries = index
            .tvdb_to_entries
            .values()
            .map(|group| group.len())
            .sum::<usize>();
        let index = Arc::new(index);

        let temp_path = self.path.with_extension("json.tmp");
        fs::write(&temp_path, &bytes)
            .await
            .map_err(|source| MappingError::Write {
                source,
                path: temp_path.clone(),
            })?;

        match fs::rename(&temp_path, &self.path).await {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                fs::remove_file(&self.path)
                    .await
                    .map_err(|source| MappingError::Remove {
                        source,
                        path: self.path.clone(),
                    })?;
                fs::rename(&temp_path, &self.path)
                    .await
                    .map_err(|source| MappingError::Write {
                        source,
                        path: self.path.clone(),
                    })?;
            }
            Err(source) => {
                return Err(MappingError::Write {
                    source,
                    path: self.path.clone(),
                });
            }
        }

        if let Some(ref etag) = new_etag {
            fs::write(&etag_path, etag.as_bytes().to_vec())
                .await
                .map_err(|source| MappingError::Write {
                    source,
                    path: etag_path.clone(),
                })?;
        } else if let Err(error) = fs::remove_file(&etag_path).await
            && error.kind() != ErrorKind::NotFound
        {
            return Err(MappingError::Remove {
                source: error,
                path: etag_path.clone(),
            });
        }

        let metadata = fs::metadata(&self.path)
            .await
            .map_err(|source| MappingError::Metadata {
                source,
                path: self.path.clone(),
            })?;
        let modified = metadata
            .modified()
            .map_err(|source| MappingError::Metadata {
                source,
                path: self.path.clone(),
            })?;

        {
            let mut guard = self.cache.write().await;
            *guard = Some(CachedMappings {
                modified,
                etag: new_etag.clone(),
                entries: index.clone(),
            });
        }

        debug!(
            path = %self.path.display(),
            url = %self.source_url,
            series,
            entries,
            "refreshed plexanibridge mappings"
        );

        Ok(())
    }

    async fn load_mappings(&self) -> Result<Arc<MappingIndex>, MappingError> {
        let metadata = match fs::metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == ErrorKind::NotFound => {
                return Err(MappingError::Read {
                    source,
                    path: self.path.clone(),
                });
            }
            Err(source) => {
                return Err(MappingError::Metadata {
                    source,
                    path: self.path.clone(),
                });
            }
        };

        let modified = metadata
            .modified()
            .map_err(|source| MappingError::Metadata {
                source,
                path: self.path.clone(),
            })?;

        let etag_path = self.etag_path();
        let etag = match fs::read_to_string(&etag_path).await {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => {
                warn!(
                    error = %error,
                    path = %etag_path.display(),
                    "failed to read cached etag while loading mappings"
                );
                None
            }
        };

        {
            let guard = self.cache.read().await;
            if let Some(cache) = guard.as_ref()
                && cache.modified == modified
            {
                debug!(
                    path = %self.path.display(),
                    "using cached plexanibridge mappings"
                );
                return Ok(cache.entries.clone());
            }
        }

        let contents = fs::read(&self.path)
            .await
            .map_err(|source| MappingError::Read {
                source,
                path: self.path.clone(),
            })?;

        let index = task::spawn_blocking(move || {
            let raw: HashMap<String, RawMappingRecord> = serde_json::from_slice(&contents)?;
            Ok::<MappingIndex, MappingError>(Self::build_index(raw))
        })
        .await??;
        let series = index.tvdb_to_entries.len();
        let entries = index
            .tvdb_to_entries
            .values()
            .map(|group| group.len())
            .sum::<usize>();
        let index = Arc::new(index);

        {
            let mut guard = self.cache.write().await;
            *guard = Some(CachedMappings {
                modified,
                etag,
                entries: index.clone(),
            });
        }

        debug!(
            path = %self.path.display(),
            series,
            entries,
            "loaded plexanibridge mappings from disk"
        );

        Ok(index)
    }

    fn etag_path(&self) -> PathBuf {
        let mut path = self.path.clone();
        path.set_extension("etag");
        path
    }

    fn build_index(raw: HashMap<String, RawMappingRecord>) -> MappingIndex {
        let mut tvdb_index: HashMap<i64, Vec<MappingEntry>> = HashMap::new();
        let mut anilist_index: HashMap<i64, Vec<ReverseMappingEntry>> = HashMap::new();
        let mut tmdb_index: HashMap<i64, i64> = HashMap::new();
        let mut anilist_tmdb: HashMap<i64, i64> = HashMap::new();

        for (anilist_id_str, record) in raw {
            let Ok(anilist_id) = anilist_id_str.parse::<i64>() else {
                debug!(
                    anilist_id = %anilist_id_str,
                    "skipping mapping with non-numeric anilist id"
                );
                continue;
            };

            let RawMappingRecord {
                tvdb_id,
                tmdb_movie_id,
                tvdb_mappings,
            } = record;

            if let Some(tvdb_id) = tvdb_id {
                if tvdb_mappings.is_empty() {
                    trace!(anilist_id, tvdb_id, "skipping mapping with no season data");
                } else {
                    let seasons = tvdb_mappings.into_keys().collect::<Vec<_>>();
                    tvdb_index.entry(tvdb_id).or_default().push(MappingEntry {
                        anilist_id,
                        seasons: seasons.clone(),
                    });
                    anilist_index
                        .entry(anilist_id)
                        .or_default()
                        .push(ReverseMappingEntry { tvdb_id, seasons });
                }
            }

            if let Some(tmdb_id) = tmdb_movie_id.and_then(|value| value.into_first()) {
                tmdb_index.insert(tmdb_id, anilist_id);
                anilist_tmdb.insert(anilist_id, tmdb_id);
            }
        }

        MappingIndex {
            tvdb_to_entries: tvdb_index,
            anilist_to_entries: anilist_index,
            tmdb_to_anilist: tmdb_index,
            anilist_to_tmdb: anilist_tmdb,
        }
    }

    pub async fn resolve_anilist_id(
        &self,
        tvdb_id: i64,
        season: u32,
    ) -> Result<Option<i64>, MappingError> {
        let mappings = self.load_mappings().await?;
        let season_key = format!("s{season}");

        if let Some(entries) = mappings.tvdb_to_entries.get(&tvdb_id) {
            debug!(
                tvdb_id,
                season,
                candidates = entries.len(),
                "found candidate mappings for tvdb id"
            );

            for entry in entries {
                if entry.seasons.iter().any(|key| key == &season_key) {
                    debug!(
                        tvdb_id,
                        season,
                        anilist_id = entry.anilist_id,
                        "matched mapping entry for season"
                    );
                    return Ok(Some(entry.anilist_id));
                }
            }
        }

        debug!(
            tvdb_id,
            season,
            path = %self.path.display(),
            "no season-specific mapping found in local mappings file"
        );

        Ok(None)
    }

    pub async fn resolve_anilist_id_for_tvdb(
        &self,
        tvdb_id: i64,
    ) -> Result<Option<i64>, MappingError> {
        let mappings = self.load_mappings().await?;
        let Some(entries) = mappings.tvdb_to_entries.get(&tvdb_id) else {
            debug!(tvdb_id, "no entries found for tvdb id");
            return Ok(None);
        };

        let mut best: Option<(i64, u32)> = None;
        for entry in entries {
            let mut seasons: Vec<u32> = entry
                .seasons
                .iter()
                .filter_map(|key| parse_season_key(key))
                .collect();

            let season = if seasons.is_empty() {
                u32::MAX
            } else {
                seasons.sort_unstable();
                seasons[0]
            };

            match best {
                Some((_, best_season)) if season >= best_season => {}
                _ => best = Some((entry.anilist_id, season)),
            }
        }

        if let Some((anilist_id, season)) = best {
            debug!(
                tvdb_id,
                anilist_id, season, "selected mapping for tv search"
            );
            return Ok(Some(anilist_id));
        }

        debug!(tvdb_id, "failed to select mapping for movie search");
        Ok(None)
    }

    pub async fn resolve_anilist_id_for_tmdb(
        &self,
        tmdb_id: i64,
    ) -> Result<Option<i64>, MappingError> {
        let mappings = self.load_mappings().await?;
        if let Some(anilist_id) = mappings.tmdb_to_anilist.get(&tmdb_id) {
            debug!(tmdb_id, anilist_id, "resolved tmdb mapping");
            Ok(Some(*anilist_id))
        } else {
            debug!(tmdb_id, "no tmdb mapping found");
            Ok(None)
        }
    }

    pub async fn resolve_tmdb_id(&self, anilist_id: i64) -> Result<Option<i64>, MappingError> {
        let mappings = self.load_mappings().await?;
        Ok(mappings.anilist_to_tmdb.get(&anilist_id).copied())
    }

    pub async fn resolve_tvdb_mappings(
        &self,
        anilist_id: i64,
    ) -> Result<Vec<TvdbMapping>, MappingError> {
        let mappings = self.load_mappings().await?;

        let result = mappings
            .anilist_to_entries
            .get(&anilist_id)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| TvdbMapping {
                        tvdb_id: entry.tvdb_id,
                        seasons: entry.seasons.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(result)
    }
}

pub(crate) fn parse_season_key(key: &str) -> Option<u32> {
    if !key.starts_with('s') {
        return None;
    }

    let digits: String = key[1..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }

    digits.parse().ok()
}

#[derive(Debug, Error)]
pub enum MappingError {
    #[error("failed to download plexanibridge mappings from {url}")]
    Download {
        #[source]
        source: reqwest::Error,
        url: Url,
    },
    #[error("failed to read mapping file at {path}")]
    Read {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to write mapping file at {path}")]
    Write {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to remove mapping file at {path}")]
    Remove {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to inspect mapping file metadata at {path}")]
    Metadata {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to deserialise plexanibridge mapping file")]
    Deserialisation(#[from] serde_json::Error),
    #[error("background task failed")]
    TaskJoin(#[from] tokio::task::JoinError),
}
