use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use reqwest::{
    Client, StatusCode,
    header::{ETAG, IF_NONE_MATCH},
};
use tokio::fs;
use tokio::sync::RwLock;
use tokio::task;
use tracing::{debug, trace, warn};

use crate::http;

pub const SOURCE_URL: &str =
    "https://github.com/anibridge/anibridge-mappings/releases/latest/download/mappings.min.json";
pub const REFRESH_INTERVAL: Duration = Duration::from_hours(6);

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

#[derive(Debug, Clone)]
pub struct PlexAniBridgeMappings {
    path: PathBuf,
    cache: Arc<RwLock<Option<CachedMappings>>>,
    client: Client,
}

impl PlexAniBridgeMappings {
    pub async fn bootstrap(data_path: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_path).await.context(format!(
            "failed to create data directory at {}",
            data_path.display()
        ))?;

        let path = data_path.join("mappings.json");
        let client = Client::builder()
            .connect_timeout(http::MAPPINGS_CONNECT_TIMEOUT)
            .read_timeout(http::MAPPINGS_READ_TIMEOUT)
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to construct PlexAniBridge HTTP client")?;

        let mappings = Self {
            path,
            cache: Arc::new(RwLock::new(None)),
            client,
        };

        if let Err(err) = mappings.refresh_mappings().await {
            warn!(
                error = %err,
                url = %SOURCE_URL,
                "failed to download mappings on startup; attempting to load from disk cache"
            );
            mappings
                .load_mappings()
                .await
                .map_err(|_| err)
                .context("failed to download mappings and no disk cache available")?;
        }
        mappings.spawn_refresh_task();

        Ok(mappings)
    }

    fn spawn_refresh_task(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REFRESH_INTERVAL).await;
                if let Err(error) = this.refresh_mappings().await {
                    warn!(
                        error = %error,
                        url = %SOURCE_URL,
                        "failed to refresh plexanibridge mappings"
                    );
                }
            }
        });
    }

    async fn refresh_mappings(&self) -> Result<()> {
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

        let mut request = self.client.get(SOURCE_URL);
        if let Some(etag) = cached_etag {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request.send().await.context(format!(
            "failed to download plexanibridge mappings from {SOURCE_URL}"
        ))?;

        if response.status() == StatusCode::NOT_MODIFIED {
            trace!(
                path = %self.path.display(),
                url = %SOURCE_URL,
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

        let response = response.error_for_status().context(format!(
            "failed to download plexanibridge mappings from {SOURCE_URL}"
        ))?;

        let new_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_owned());

        let bytes = response
            .bytes()
            .await
            .context(format!(
                "failed to download plexanibridge mappings from {SOURCE_URL}"
            ))?
            .to_vec();

        // Offload heavy JSON deserialisation and index build to a blocking thread so the
        // async runtime worker threads aren't stalled by CPU work.
        let index = {
            let bytes = bytes.clone();
            task::spawn_blocking(move || -> Result<MappingIndex> {
                let raw: HashMap<String, serde_json::Value> = serde_json::from_slice(&bytes)
                    .context("failed to deserialise plexanibridge mapping file")?;
                Ok(Self::build_index(raw))
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
        fs::write(&temp_path, &bytes).await.context(format!(
            "failed to write mapping file at {}",
            temp_path.display()
        ))?;

        match fs::rename(&temp_path, &self.path).await {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                fs::remove_file(&self.path).await.context(format!(
                    "failed to remove mapping file at {}",
                    self.path.display()
                ))?;
                fs::rename(&temp_path, &self.path).await.context(format!(
                    "failed to write mapping file at {}",
                    self.path.display()
                ))?;
            }
            Err(err) => {
                bail!(
                    "failed to write mapping file at {}: {err}",
                    self.path.display()
                );
            }
        }

        if let Some(ref etag) = new_etag {
            fs::write(&etag_path, etag.as_bytes().to_vec())
                .await
                .context(format!(
                    "failed to write mapping file at {}",
                    etag_path.display()
                ))?;
        } else if let Err(err) = fs::remove_file(&etag_path).await
            && err.kind() != ErrorKind::NotFound
        {
            bail!(
                "failed to remove mapping file at {}: {err}",
                etag_path.display()
            );
        }

        let metadata = fs::metadata(&self.path).await.context(format!(
            "failed to inspect mapping file metadata at {}",
            self.path.display()
        ))?;
        let modified = metadata.modified().context(format!(
            "failed to inspect mapping file metadata at {}",
            self.path.display()
        ))?;

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
            url = %SOURCE_URL,
            series,
            entries,
            "refreshed plexanibridge mappings"
        );

        Ok(())
    }

    async fn load_mappings(&self) -> Result<Arc<MappingIndex>> {
        let metadata = match fs::metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                bail!(
                    "failed to read mapping file at {}: {err}",
                    self.path.display()
                );
            }
            Err(err) => {
                bail!(
                    "failed to inspect mapping file metadata at {}: {err}",
                    self.path.display()
                );
            }
        };

        let modified = metadata.modified().context(format!(
            "failed to inspect mapping file metadata at {}",
            self.path.display()
        ))?;

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
                trace!(
                    path = %self.path.display(),
                    "using cached plexanibridge mappings"
                );
                return Ok(cache.entries.clone());
            }
        }

        let contents = fs::read(&self.path).await.context(format!(
            "failed to read mapping file at {}",
            self.path.display()
        ))?;

        let index = task::spawn_blocking(move || -> Result<MappingIndex> {
            let raw: HashMap<String, serde_json::Value> = serde_json::from_slice(&contents)
                .context("failed to deserialise plexanibridge mapping file")?;
            Ok(Self::build_index(raw))
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

    fn build_index(raw: HashMap<String, serde_json::Value>) -> MappingIndex {
        let mut tvdb_index: HashMap<i64, Vec<MappingEntry>> = HashMap::new();
        let mut anilist_index: HashMap<i64, Vec<ReverseMappingEntry>> = HashMap::new();
        let mut tmdb_index: HashMap<i64, i64> = HashMap::new();
        let mut anilist_tmdb: HashMap<i64, i64> = HashMap::new();

        for (source_key, targets_value) in raw {
            if source_key.starts_with('$') {
                continue;
            }

            let Some(("anilist", id_str, _scope)) = parse_descriptor(&source_key) else {
                continue;
            };

            let Ok(anilist_id) = id_str.parse::<i64>() else {
                debug!(source_key, "skipping mapping with non-numeric anilist id");
                continue;
            };

            let Some(targets) = targets_value.as_object() else {
                continue;
            };

            for target_key in targets.keys() {
                let Some((target_provider, target_id_str, target_scope)) =
                    parse_descriptor(target_key)
                else {
                    continue;
                };

                if target_provider == "tvdb_show" {
                    let Ok(tvdb_id) = target_id_str.parse::<i64>() else {
                        continue;
                    };
                    let season = target_scope.unwrap_or("s1").to_owned();
                    tvdb_index.entry(tvdb_id).or_default().push(MappingEntry {
                        anilist_id,
                        seasons: vec![season.clone()],
                    });
                    anilist_index
                        .entry(anilist_id)
                        .or_default()
                        .push(ReverseMappingEntry {
                            tvdb_id,
                            seasons: vec![season],
                        });
                } else if target_provider == "tmdb_movie" {
                    let Ok(tmdb_id) = target_id_str.parse::<i64>() else {
                        continue;
                    };
                    tmdb_index.insert(tmdb_id, anilist_id);
                    anilist_tmdb.insert(anilist_id, tmdb_id);
                }
            }
        }

        MappingIndex {
            tvdb_to_entries: tvdb_index,
            anilist_to_entries: anilist_index,
            tmdb_to_anilist: tmdb_index,
            anilist_to_tmdb: anilist_tmdb,
        }
    }

    pub async fn resolve_anilist_id(&self, tvdb_id: i64, season: u32) -> Result<Option<i64>> {
        let mappings = self.load_mappings().await?;
        let season_key = format!("s{season}");

        if let Some(entries) = mappings.tvdb_to_entries.get(&tvdb_id) {
            trace!(
                tvdb_id,
                season,
                candidates = entries.len(),
                "found candidate mappings for tvdb id"
            );

            for entry in entries {
                if entry.seasons.iter().any(|key| key == &season_key) {
                    trace!(
                        tvdb_id,
                        season,
                        anilist_id = entry.anilist_id,
                        "matched mapping entry for season"
                    );
                    return Ok(Some(entry.anilist_id));
                }
            }
        }

        trace!(
            tvdb_id,
            season,
            path = %self.path.display(),
            "no season-specific mapping found in local mappings file"
        );

        Ok(None)
    }

    pub async fn resolve_anilist_id_for_tvdb(&self, tvdb_id: i64) -> Result<Option<i64>> {
        let mappings = self.load_mappings().await?;
        let Some(entries) = mappings.tvdb_to_entries.get(&tvdb_id) else {
            trace!(tvdb_id, "no entries found for tvdb id");
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
            trace!(
                tvdb_id,
                anilist_id, season, "selected mapping for tv search"
            );
            return Ok(Some(anilist_id));
        }

        trace!(tvdb_id, "failed to select mapping for movie search");
        Ok(None)
    }

    pub async fn resolve_anilist_id_for_tmdb(&self, tmdb_id: i64) -> Result<Option<i64>> {
        let mappings = self.load_mappings().await?;
        if let Some(anilist_id) = mappings.tmdb_to_anilist.get(&tmdb_id) {
            trace!(tmdb_id, anilist_id, "resolved tmdb mapping");
            Ok(Some(*anilist_id))
        } else {
            trace!(tmdb_id, "no tmdb mapping found");
            Ok(None)
        }
    }

    pub async fn resolve_tmdb_id(&self, anilist_id: i64) -> Result<Option<i64>> {
        let mappings = self.load_mappings().await?;
        Ok(mappings.anilist_to_tmdb.get(&anilist_id).copied())
    }

    pub async fn resolve_tvdb_mappings(&self, anilist_id: i64) -> Result<Vec<TvdbMapping>> {
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

fn parse_descriptor(key: &str) -> Option<(&str, &str, Option<&str>)> {
    let mut parts = key.splitn(3, ':');
    let provider = parts.next()?;
    let id = parts.next()?;
    let scope = parts.next();
    Some((provider, id, scope))
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
