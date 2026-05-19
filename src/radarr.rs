use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task;
use tracing::trace;

use crate::{config::RadarrConfig, http};

const CACHE_FILENAME: &str = "radarr_titles.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadarrMovie {
    pub title: String,
    pub year: u32,
}

#[derive(Debug, Clone)]
pub struct RadarrClient {
    http: Client,
    config: RadarrConfig,
    cache: Arc<RwLock<HashMap<i64, RadarrMovie>>>,
    cache_path: PathBuf,
}

impl RadarrClient {
    pub fn new(config: RadarrConfig, data_path: PathBuf) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(http::TIMEOUT)
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()?;

        let cache_path = data_path.join(CACHE_FILENAME);

        let cache = load_cache(&cache_path)?;

        Ok(Self {
            http,
            config,
            cache: Arc::new(RwLock::new(cache)),
            cache_path,
        })
    }

    pub async fn resolve_name(&self, tmdb_id: i64) -> Result<RadarrMovie, RadarrError> {
        if let Some(existing) = self.cached_movie(tmdb_id).await {
            trace!(tmdb_id, "using cached Radarr title");
            return Ok(existing);
        }

        let mut url = self
            .config
            .url
            .join("api/v3/movie/lookup/tmdb")
            .map_err(RadarrError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("tmdbId", &tmdb_id.to_string());
        }

        trace!(tmdb_id, url = %url, "requesting Radarr movie lookup");

        let response = self
            .http
            .get(url)
            .header("X-Api-Key", &self.config.api_key)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let is_not_found =
                status == reqwest::StatusCode::NOT_FOUND || body.contains("was not found");
            if is_not_found {
                trace!(
                    tmdb_id,
                    status = status.as_u16(),
                    "Radarr movie not found on TMDb"
                );
                return Err(RadarrError::NotFound { tmdb_id });
            }
            tracing::warn!(
                tmdb_id,
                status = status.as_u16(),
                body = %body,
                "Radarr movie lookup returned non-success status"
            );
            return Err(RadarrError::Api {
                tmdb_id,
                status: status.as_u16(),
                body,
            });
        }

        let payload: MovieLookupEntry = response.json().await?;

        let Some(title) = payload.title else {
            return Err(RadarrError::NotFound { tmdb_id });
        };

        let Some(year) = payload.year else {
            trace!(tmdb_id, "skipping Radarr movie lookup due to missing year");
            return Err(RadarrError::NotFound { tmdb_id });
        };

        let movie = RadarrMovie { title, year };

        self.store_movie(tmdb_id, &movie).await?;

        Ok(movie)
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

    async fn cached_movie(&self, tmdb_id: i64) -> Option<RadarrMovie> {
        let guard = self.cache.read().await;
        guard.get(&tmdb_id).cloned()
    }

    async fn store_movie(&self, tmdb_id: i64, movie: &RadarrMovie) -> Result<(), RadarrError> {
        {
            let mut guard = self.cache.write().await;
            guard.insert(tmdb_id, movie.clone());
        }
        self.persist_cache().await
    }

    async fn persist_cache(&self) -> Result<(), RadarrError> {
        // Clone snapshot while holding the lock then offload CPU + IO to blocking thread.
        let snapshot = {
            let guard = self.cache.read().await;
            guard.clone()
        };

        let path = self.cache_path.clone();

        let result = task::spawn_blocking(
            move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let json = serde_json::to_vec_pretty(&snapshot)?;

                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                std::fs::write(&path, json)?;

                Ok(())
            },
        )
        .await
        .map_err(|source| RadarrError::CacheWrite {
            source: std::io::Error::other(format!("join error: {source}")),
            path: self.cache_path.clone(),
        })?;

        if let Err(_err) = result {
            return Err(RadarrError::CacheWrite {
                source: std::io::Error::other("failed to persist cache"),
                path: self.cache_path.clone(),
            });
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct MovieLookupEntry {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<u32>,
}

fn load_cache(path: &Path) -> Result<HashMap<i64, RadarrMovie>, RadarrError> {
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

    let data: HashMap<i64, RadarrMovie> =
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
    #[error("Radarr api returned {status} for tmdb {tmdb_id}: {body}")]
    Api {
        tmdb_id: i64,
        status: u16,
        body: String,
    },
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
    #[error("failed to create cache directory at {path}")]
    CacheDir {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_config() -> RadarrConfig {
        RadarrConfig {
            url: reqwest::Url::parse("http://localhost:7878/").unwrap(),
            api_key: "test-key".to_string(),
        }
    }

    fn make_client(dir: &TempDir) -> RadarrClient {
        RadarrClient::new(make_config(), dir.path().to_path_buf())
            .expect("client construction must succeed")
    }

    fn movie(title: &str, year: u32) -> RadarrMovie {
        RadarrMovie {
            title: title.to_string(),
            year,
        }
    }

    #[test]
    fn loads_cache_from_disk() {
        let dir = TempDir::new().unwrap();

        let missing = dir.path().join("missing.json");
        assert!(load_cache(&missing).unwrap().is_empty());

        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "").unwrap();
        assert!(load_cache(&empty).unwrap().is_empty());

        let populated = dir.path().join("populated.json");
        std::fs::write(
            &populated,
            r#"{"42":{"title":"Spirited Away","year":2001}}"#,
        )
        .unwrap();
        let cache = load_cache(&populated).unwrap();
        let entry = cache.get(&42).expect("entry must exist");
        assert_eq!(entry.title, "Spirited Away");
        assert_eq!(entry.year, 2001);

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(matches!(
            load_cache(&bad),
            Err(RadarrError::CacheParse { .. })
        ));
    }

    #[tokio::test]
    async fn stores_and_retrieves_movies() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        assert!(client.cached_movie(42).await.is_none());

        client
            .store_movie(42, &movie("Spirited Away", 2001))
            .await
            .unwrap();

        let cached = client.cached_movie(42).await.expect("must be cached");
        assert_eq!(cached.title, "Spirited Away");
        assert_eq!(cached.year, 2001);

        let reloaded = load_cache(&dir.path().join(CACHE_FILENAME)).unwrap();
        let entry = reloaded.get(&42).expect("must persist");
        assert_eq!(entry.title, "Spirited Away");
        assert_eq!(entry.year, 2001);
    }

    #[tokio::test]
    async fn retain_titles_clears_when_keep_empty() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        client.store_movie(1, &movie("A", 2001)).await.unwrap();
        client.store_movie(2, &movie("B", 2002)).await.unwrap();

        client.retain_titles(&HashSet::new()).await.unwrap();

        assert!(client.cached_movie(1).await.is_none());
        assert!(client.cached_movie(2).await.is_none());
        assert!(
            load_cache(&dir.path().join(CACHE_FILENAME))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn retain_titles_drops_unlisted_entries() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        client.store_movie(1, &movie("A", 2001)).await.unwrap();
        client.store_movie(2, &movie("B", 2002)).await.unwrap();
        client.store_movie(3, &movie("C", 2003)).await.unwrap();

        let keep: HashSet<i64> = [1, 3].into_iter().collect();
        client.retain_titles(&keep).await.unwrap();

        assert!(client.cached_movie(1).await.is_some());
        assert!(client.cached_movie(2).await.is_none());
        assert!(client.cached_movie(3).await.is_some());
    }

    #[tokio::test]
    async fn retain_titles_skips_persist_when_unchanged() {
        // Superset keep on populated cache: nothing removed, no rewrite expected.
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        client.store_movie(1, &movie("A", 2001)).await.unwrap();

        let cache_path = dir.path().join(CACHE_FILENAME);
        std::fs::remove_file(&cache_path).unwrap();

        let keep: HashSet<i64> = [1, 2].into_iter().collect();
        client.retain_titles(&keep).await.unwrap();
        assert!(!cache_path.exists());

        // Empty keep on empty cache: same short-circuit.
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        client.retain_titles(&HashSet::new()).await.unwrap();
        assert!(!dir.path().join(CACHE_FILENAME).exists());
    }
}
