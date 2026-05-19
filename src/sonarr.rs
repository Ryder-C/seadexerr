use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task;
use tracing::{trace, warn};

use crate::{config::SonarrConfig, http};

const CACHE_FILENAME: &str = "sonarr_titles.json";

#[derive(Debug, Clone)]
pub struct SonarrClient {
    http: Client,
    config: SonarrConfig,
    cache: Arc<RwLock<HashMap<i64, String>>>,
    cache_path: PathBuf,
}

impl SonarrClient {
    pub fn new(config: SonarrConfig, data_path: PathBuf) -> anyhow::Result<Self> {
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

    pub async fn resolve_name(&self, tvdb_id: i64) -> Result<String, SonarrError> {
        if let Some(cached) = self.cached_title(tvdb_id).await {
            trace!(tvdb_id, "using cached Sonarr title");
            return Ok(cached);
        }

        let mut url = self
            .config
            .url
            .join("api/v3/series/lookup")
            .map_err(SonarrError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("term", &format!("tvdb:{tvdb_id}"));
        }

        trace!(
            tvdb_id,
            url = %url,
            "requesting Sonarr series lookup"
        );

        let response = self
            .http
            .get(url)
            .header("X-Api-Key", &self.config.api_key)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            warn!(
                tvdb_id,
                status = status.as_u16(),
                body = %body,
                "Sonarr series lookup returned non-success status"
            );
            return Err(SonarrError::Api {
                tvdb_id,
                status: status.as_u16(),
                body,
            });
        }

        let payload: Vec<SeriesLookupEntry> = response.json().await?;

        trace!(
            tvdb_id,
            results = payload.len(),
            "Sonarr series lookup response received"
        );

        let Some(title) = payload.into_iter().find_map(|entry| entry.title) else {
            return Err(SonarrError::NotFound { tvdb_id });
        };

        self.store_title(tvdb_id, &title).await?;

        Ok(title)
    }

    pub async fn retain_titles(&self, keep: &HashSet<i64>) -> Result<(), SonarrError> {
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
        guard.retain(|tvdb_id, _| keep.contains(tvdb_id));

        if guard.len() == original_len {
            return Ok(());
        }

        drop(guard);
        self.persist_cache().await
    }

    async fn cached_title(&self, tvdb_id: i64) -> Option<String> {
        let guard = self.cache.read().await;
        guard.get(&tvdb_id).cloned()
    }

    async fn store_title(&self, tvdb_id: i64, title: &str) -> Result<(), SonarrError> {
        {
            let mut guard = self.cache.write().await;
            guard.insert(tvdb_id, title.to_string());
        }
        self.persist_cache().await
    }

    async fn persist_cache(&self) -> Result<(), SonarrError> {
        // Clone snapshot under the read lock, then offload serialization + write
        // to a blocking thread to avoid blocking tokio worker threads.
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
        .map_err(|source| SonarrError::CacheWrite {
            source: std::io::Error::other(format!("join error: {source}")),
            path: self.cache_path.clone(),
        })?;

        if let Err(_err) = result {
            // For simplicity, map any persistence error to CacheWrite. We avoid trying to
            // downcast boxed errors back to concrete types here.
            return Err(SonarrError::CacheWrite {
                source: std::io::Error::other("failed to persist cache"),
                path: self.cache_path.clone(),
            });
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct SeriesLookupEntry {
    #[serde(default)]
    title: Option<String>,
}

fn load_cache(path: &Path) -> Result<HashMap<i64, String>, SonarrError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SonarrError::CacheDir {
            source,
            path: parent.to_path_buf(),
        })?;
    }

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => {
            return Err(SonarrError::CacheRead {
                source,
                path: path.to_path_buf(),
            });
        }
    };

    if bytes.is_empty() {
        return Ok(HashMap::new());
    }

    let data: HashMap<i64, String> =
        serde_json::from_slice(&bytes).map_err(|source| SonarrError::CacheParse {
            source,
            path: path.to_path_buf(),
        })?;

    Ok(data)
}

#[derive(Debug, Error)]
pub enum SonarrError {
    #[error("failed to build Sonarr request url")]
    Url(#[from] url::ParseError),
    #[error("http error when querying Sonarr api")]
    Http(#[from] reqwest::Error),
    #[error("Sonarr api returned {status} for tvdb {tvdb_id}: {body}")]
    Api {
        tvdb_id: i64,
        status: u16,
        body: String,
    },
    #[error("no Sonarr series title found for tvdb {tvdb_id}")]
    NotFound { tvdb_id: i64 },
    #[error("failed to read cached Sonarr titles at {path}")]
    CacheRead {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to write cached Sonarr titles at {path}")]
    CacheWrite {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to parse cached Sonarr titles at {path}")]
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

    fn make_config() -> SonarrConfig {
        SonarrConfig {
            url: reqwest::Url::parse("http://localhost:8989/").unwrap(),
            api_key: "test-key".to_string(),
        }
    }

    fn make_client(dir: &TempDir) -> SonarrClient {
        SonarrClient::new(make_config(), dir.path().to_path_buf())
            .expect("client construction must succeed")
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
        std::fs::write(&populated, r#"{"42":"Naruto","99":"Bleach"}"#).unwrap();
        let cache = load_cache(&populated).unwrap();
        assert_eq!(cache.get(&42), Some(&"Naruto".to_string()));
        assert_eq!(cache.get(&99), Some(&"Bleach".to_string()));

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(matches!(
            load_cache(&bad),
            Err(SonarrError::CacheParse { .. })
        ));
    }

    #[tokio::test]
    async fn stores_and_retrieves_titles() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        assert_eq!(client.cached_title(42).await, None);

        client.store_title(42, "Naruto").await.unwrap();
        assert_eq!(client.cached_title(42).await, Some("Naruto".to_string()));

        let reloaded = load_cache(&dir.path().join(CACHE_FILENAME)).unwrap();
        assert_eq!(reloaded.get(&42), Some(&"Naruto".to_string()));
    }

    #[tokio::test]
    async fn retain_titles_clears_when_keep_empty() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        client.store_title(1, "A").await.unwrap();
        client.store_title(2, "B").await.unwrap();

        client.retain_titles(&HashSet::new()).await.unwrap();

        assert_eq!(client.cached_title(1).await, None);
        assert_eq!(client.cached_title(2).await, None);
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

        client.store_title(1, "A").await.unwrap();
        client.store_title(2, "B").await.unwrap();
        client.store_title(3, "C").await.unwrap();

        let keep: HashSet<i64> = [1, 3].into_iter().collect();
        client.retain_titles(&keep).await.unwrap();

        assert_eq!(client.cached_title(1).await, Some("A".to_string()));
        assert_eq!(client.cached_title(2).await, None);
        assert_eq!(client.cached_title(3).await, Some("C".to_string()));
    }

    #[tokio::test]
    async fn retain_titles_skips_persist_when_unchanged() {
        // Superset keep on populated cache: nothing removed, no rewrite expected.
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        client.store_title(1, "A").await.unwrap();

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
