use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::{fs as async_fs, sync::RwLock};
use tracing::debug;
use url::Url;

#[derive(Debug, Clone)]
pub struct RadarrClient {
    http: Client,
    base_url: Url,
    api_key: String,
    cache: Arc<RwLock<HashMap<i64, String>>>,
    cache_path: PathBuf,
}

impl RadarrClient {
    pub fn new(
        base_url: Url,
        api_key: String,
        timeout: Duration,
        cache_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()?;

        let cache = load_cache(&cache_path)?;

        Ok(Self {
            http,
            base_url,
            api_key,
            cache: Arc::new(RwLock::new(cache)),
            cache_path,
        })
    }

    pub async fn resolve_name(&self, tmdb_id: i64) -> Result<String, RadarrError> {
        if let Some(existing) = self.cached_title(tmdb_id).await {
            debug!(tmdb_id, "using cached Radarr title");
            return Ok(existing);
        }

        let mut url = self
            .base_url
            .join("api/v3/movie/lookup/tmdb")
            .map_err(RadarrError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("tmdbId", &tmdb_id.to_string());
        }

        debug!(tmdb_id, url = %url, "requesting Radarr movie lookup");

        let response = self
            .http
            .get(url)
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?
            .error_for_status()?;

        let payload: MovieLookupEntry = response.json().await?;

        let Some(title) = payload.title else {
            return Err(RadarrError::NotFound { tmdb_id });
        };

        self.store_title(tmdb_id, &title).await?;

        Ok(title)
    }

    pub async fn retain_titles(&self, keep: &HashSet<i64>) -> Result<(), RadarrError> {
        if keep.is_empty() {
            let mut guard = self.cache.write().await;
            if guard.is_empty() {
                return Ok(());
            }
            guard.clear();
            drop(guard);
            return self.persist_cache().await;
        }

        let mut guard = self.cache.write().await;
        let original_len = guard.len();
        guard.retain(|tmdb_id, _| keep.contains(tmdb_id));

        if guard.len() == original_len {
            return Ok(());
        }

        drop(guard);
        self.persist_cache().await
    }

    async fn cached_title(&self, tmdb_id: i64) -> Option<String> {
        let guard = self.cache.read().await;
        guard.get(&tmdb_id).cloned()
    }

    async fn store_title(&self, tmdb_id: i64, title: &str) -> Result<(), RadarrError> {
        {
            let mut guard = self.cache.write().await;
            guard.insert(tmdb_id, title.to_string());
        }
        self.persist_cache().await
    }

    async fn persist_cache(&self) -> Result<(), RadarrError> {
        let snapshot = {
            let guard = self.cache.read().await;
            guard.clone()
        };

        let json = serde_json::to_vec_pretty(&snapshot).map_err(RadarrError::CacheSerialise)?;

        if let Some(parent) = self.cache_path.parent() {
            async_fs::create_dir_all(parent)
                .await
                .map_err(|source| RadarrError::CacheDir {
                    source,
                    path: parent.to_path_buf(),
                })?;
        }

        async_fs::write(&self.cache_path, json)
            .await
            .map_err(|source| RadarrError::CacheWrite {
                source,
                path: self.cache_path.clone(),
            })?;

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct MovieLookupEntry {
    #[serde(default)]
    title: Option<String>,
}

fn load_cache(path: &Path) -> Result<HashMap<i64, String>, RadarrError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RadarrError::CacheDir {
            source,
            path: parent.to_path_buf(),
        })?;
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => {
            return Err(RadarrError::CacheRead {
                source,
                path: path.to_path_buf(),
            });
        }
    };

    if bytes.is_empty() {
        return Ok(HashMap::new());
    }

    let data: HashMap<i64, String> =
        serde_json::from_slice(&bytes).map_err(|source| RadarrError::CacheParse {
            source,
            path: path.to_path_buf(),
        })?;

    Ok(data)
}

#[derive(Debug, Error)]
pub enum RadarrError {
    #[error("failed to build Radarr request url")]
    Url(#[from] url::ParseError),
    #[error("http error when querying Radarr api")]
    Http(#[from] reqwest::Error),
    #[error("no Radarr movie title found for tmdb {tmdb_id}")]
    NotFound { tmdb_id: i64 },
    #[error("failed to read cached Radarr titles at {path}")]
    CacheRead {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to write cached Radarr titles at {path}")]
    CacheWrite {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to parse cached Radarr titles at {path}")]
    CacheParse {
        #[source]
        source: serde_json::Error,
        path: PathBuf,
    },
    #[error("failed to serialise cached Radarr titles")]
    CacheSerialise(#[from] serde_json::Error),
    #[error("failed to create cache directory at {path}")]
    CacheDir {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
}
