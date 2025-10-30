use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{debug, warn};
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
    entries: Arc<HashMap<i64, Vec<MappingEntry>>>,
}

#[derive(Debug, Clone)]
struct MappingEntry {
    anilist_id: i64,
    seasons: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawMappingRecord {
    #[serde(default)]
    tvdb_id: Option<i64>,
    #[serde(default)]
    tvdb_mappings: HashMap<String, serde_json::Value>,
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
        let response = self
            .client
            .get(self.source_url.clone())
            .send()
            .await
            .map_err(|source| MappingError::Download {
                source,
                url: self.source_url.clone(),
            })?;

        let response = response
            .error_for_status()
            .map_err(|source| MappingError::Download {
                source,
                url: self.source_url.clone(),
            })?;

        let bytes = response
            .bytes()
            .await
            .map_err(|source| MappingError::Download {
                source,
                url: self.source_url.clone(),
            })?
            .to_vec();

        let raw: HashMap<String, RawMappingRecord> = serde_json::from_slice(&bytes)?;
        let index = Self::build_index(raw);
        let series = index.len();
        let entries = index.values().map(|group| group.len()).sum::<usize>();
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

    async fn load_mappings(&self) -> Result<Arc<HashMap<i64, Vec<MappingEntry>>>, MappingError> {
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

        {
            let guard = self.cache.read().await;
            if let Some(cache) = guard.as_ref() {
                if cache.modified == modified {
                    debug!(
                        path = %self.path.display(),
                        "using cached plexanibridge mappings"
                    );
                    return Ok(cache.entries.clone());
                }
            }
        }

        let contents = fs::read(&self.path)
            .await
            .map_err(|source| MappingError::Read {
                source,
                path: self.path.clone(),
            })?;

        let raw: HashMap<String, RawMappingRecord> = serde_json::from_slice(&contents)?;
        let index = Self::build_index(raw);
        let series = index.len();
        let entries = index.values().map(|group| group.len()).sum::<usize>();
        let index = Arc::new(index);

        {
            let mut guard = self.cache.write().await;
            *guard = Some(CachedMappings {
                modified,
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

    fn build_index(raw: HashMap<String, RawMappingRecord>) -> HashMap<i64, Vec<MappingEntry>> {
        let mut index: HashMap<i64, Vec<MappingEntry>> = HashMap::new();

        for (anilist_id_str, record) in raw {
            let Some(tvdb_id) = record.tvdb_id else {
                continue;
            };

            let Ok(anilist_id) = anilist_id_str.parse::<i64>() else {
                debug!(
                    anilist_id = %anilist_id_str,
                    "skipping mapping with non-numeric anilist id"
                );
                continue;
            };

            if record.tvdb_mappings.is_empty() {
                debug!(anilist_id, tvdb_id, "skipping mapping with no season data");
                continue;
            }

            let seasons = record.tvdb_mappings.into_keys().collect::<Vec<_>>();
            index.entry(tvdb_id).or_default().push(MappingEntry {
                anilist_id,
                seasons,
            });
        }

        index
    }

    pub async fn resolve_anilist_id(
        &self,
        tvdb_id: i64,
        season: u32,
    ) -> Result<Option<i64>, MappingError> {
        let mappings = self.load_mappings().await?;
        let season_key = format!("s{season}");

        if let Some(entries) = mappings.get(&tvdb_id) {
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
}
