use std::borrow::Cow;
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
use serde::de::IgnoredAny;
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

        let bytes = response.bytes().await.context(format!(
            "failed to download plexanibridge mappings from {SOURCE_URL}"
        ))?;

        // Offload heavy JSON deserialisation and index build to a blocking thread so the
        // async runtime worker threads aren't stalled by CPU work. `Bytes` is cheap to
        // clone (reference-counted), so the parser and the on-disk write below share a
        // single buffer instead of holding two copies of the whole file.
        let index = {
            let bytes = bytes.clone();
            task::spawn_blocking(move || Self::build_index(&bytes)).await??
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

    /// Returns the current mapping index. Falls back to loading from
    /// disk only if the cache was never hydrated.
    async fn index(&self) -> Result<Arc<MappingIndex>> {
        {
            let guard = self.cache.read().await;
            if let Some(cache) = guard.as_ref() {
                return Ok(cache.entries.clone());
            }
        }
        self.load_mappings().await
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

        let index = task::spawn_blocking(move || Self::build_index(&contents)).await??;
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

    fn build_index(bytes: &[u8]) -> Result<MappingIndex> {
        // Stream the cache file straight into the index
        // Saves alot of memory over deserializing the whole document into a `serde_json::Value`
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let index =
            serde::Deserializer::deserialize_map(&mut deserializer, IndexBuilder::default())
                .context("failed to deserialise plexanibridge mapping file")?;
        deserializer
            .end()
            .context("failed to deserialise plexanibridge mapping file")?;
        Ok(index)
    }

    pub async fn resolve_anilist_id(&self, tvdb_id: i64, season: u32) -> Result<Option<i64>> {
        let mappings = self.index().await?;
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

    pub async fn resolve_anilist_id_for_tmdb(&self, tmdb_id: i64) -> Result<Option<i64>> {
        let mappings = self.index().await?;
        if let Some(anilist_id) = mappings.tmdb_to_anilist.get(&tmdb_id) {
            trace!(tmdb_id, anilist_id, "resolved tmdb mapping");
            Ok(Some(*anilist_id))
        } else {
            trace!(tmdb_id, "no tmdb mapping found");
            Ok(None)
        }
    }

    pub async fn resolve_tmdb_id(&self, anilist_id: i64) -> Result<Option<i64>> {
        let mappings = self.index().await?;
        Ok(mappings.anilist_to_tmdb.get(&anilist_id).copied())
    }

    pub async fn resolve_tvdb_mappings(&self, anilist_id: i64) -> Result<Vec<TvdbMapping>> {
        let mappings = self.index().await?;

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

/// Streaming visitor over the top-level mappings object. Accumulates the four
/// index maps while discarding everything that the index doesn't need.
#[derive(Default)]
struct IndexBuilder {
    tvdb_to_entries: HashMap<i64, Vec<MappingEntry>>,
    anilist_to_entries: HashMap<i64, Vec<ReverseMappingEntry>>,
    tmdb_to_anilist: HashMap<i64, i64>,
    anilist_to_tmdb: HashMap<i64, i64>,
}

impl IndexBuilder {
    fn record_target(&mut self, anilist_id: i64, target_key: &str) {
        let Some((provider, id_str, scope)) = parse_descriptor(target_key) else {
            return;
        };

        if provider == "tvdb_show" {
            let Ok(tvdb_id) = id_str.parse::<i64>() else {
                return;
            };
            let season = scope.unwrap_or("s1").to_owned();
            self.tvdb_to_entries
                .entry(tvdb_id)
                .or_default()
                .push(MappingEntry {
                    anilist_id,
                    seasons: vec![season.clone()],
                });
            self.anilist_to_entries
                .entry(anilist_id)
                .or_default()
                .push(ReverseMappingEntry {
                    tvdb_id,
                    seasons: vec![season],
                });
        } else if provider == "tmdb_movie" {
            let Ok(tmdb_id) = id_str.parse::<i64>() else {
                return;
            };
            self.tmdb_to_anilist.insert(tmdb_id, anilist_id);
            self.anilist_to_tmdb.insert(anilist_id, tmdb_id);
        }
    }
}

impl<'de> serde::de::Visitor<'de> for IndexBuilder {
    type Value = MappingIndex;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a map of provider descriptors to mapping objects")
    }

    fn visit_map<A>(mut self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        while let Some(CowStr(source_key)) = map.next_key::<CowStr<'_>>()? {
            let anilist_id = if source_key.starts_with('$') {
                None
            } else {
                match parse_descriptor(&source_key) {
                    Some(("anilist", id_str, _scope)) => match id_str.parse::<i64>() {
                        Ok(id) => Some(id),
                        Err(_) => {
                            debug!(source_key = %source_key, "skipping mapping with non-numeric anilist id");
                            None
                        }
                    },
                    _ => None,
                }
            };

            match anilist_id {
                Some(anilist_id) => map.next_value_seed(TargetsSeed {
                    anilist_id,
                    builder: &mut self,
                })?,
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        Ok(MappingIndex {
            tvdb_to_entries: self.tvdb_to_entries,
            anilist_to_entries: self.anilist_to_entries,
            tmdb_to_anilist: self.tmdb_to_anilist,
            anilist_to_tmdb: self.anilist_to_tmdb,
        })
    }
}

/// Streams one source entry's target map, recording each target key and
/// discarding its value.
struct TargetsSeed<'a> {
    anilist_id: i64,
    builder: &'a mut IndexBuilder,
}

impl<'de> serde::de::DeserializeSeed<'de> for TargetsSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for TargetsSeed<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a map of target descriptors")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        while let Some(CowStr(target_key)) = map.next_key::<CowStr<'_>>()? {
            map.next_value::<IgnoredAny>()?;
            self.builder.record_target(self.anilist_id, &target_key);
        }
        Ok(())
    }
}

/// `Cow<str>` that borrows from the input when the deserializer allows it.
/// serde's own `Cow` impl always allocates an owned copy.
struct CowStr<'de>(Cow<'de, str>);

impl<'de> serde::Deserialize<'de> for CowStr<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CowStrVisitor;

        impl<'de> serde::de::Visitor<'de> for CowStrVisitor {
            type Value = CowStr<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(CowStr(Cow::Borrowed(v)))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(CowStr(Cow::Owned(v.to_owned())))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(CowStr(Cow::Owned(v)))
            }
        }

        deserializer.deserialize_str(CowStrVisitor)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_index_extracts_tvdb_and_movie_mappings() {
        let raw = br#"{
            "$meta": {"schema_version": "3.0.3"},
            "anilist:290": {
                "tvdb_show:72025:s1": {"1-13": "1-13"},
                "mal:290": {"1-13": "1-13"}
            },
            "anilist:1225": {
                "tvdb_show:70973:s2": {"1-3": "1-3"}
            },
            "anilist:500": {
                "tmdb_movie:12345": {"1": "1"}
            },
            "mal:999": {
                "tvdb_show:1:s1": {"1": "1"}
            }
        }"#;

        let index = PlexAniBridgeMappings::build_index(raw).expect("index should build");

        // tvdb_show targets are indexed both directions, keyed by season scope.
        assert_eq!(index.tvdb_to_entries.len(), 2);
        let entries = &index.tvdb_to_entries[&72025];
        assert_eq!(entries[0].anilist_id, 290);
        assert_eq!(entries[0].seasons, vec!["s1".to_string()]);
        assert_eq!(index.anilist_to_entries[&290][0].tvdb_id, 72025);

        // tmdb_movie targets populate the movie maps both directions.
        assert_eq!(index.tmdb_to_anilist[&12345], 500);
        assert_eq!(index.anilist_to_tmdb[&500], 12345);

        // `$meta` and non-anilist source keys are ignored.
        assert!(!index.tvdb_to_entries.contains_key(&1));
    }
}
