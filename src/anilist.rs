use std::{
    collections::HashMap,
    io::ErrorKind,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use strum::EnumString;
use thiserror::Error;
use tokio::{sync::RwLock, task};
use tracing::{debug, trace, warn};

const ANILIST_BASE_URL: &str = "https://graphql.anilist.co";
const MAX_IDS_PER_REQUEST: usize = 50;
const CACHE_FILENAME: &str = "anilist_formats.json";

const MEDIA_QUERY: &str = r#"
query MediaById($idIn: [Int], $perPage: Int) {
  Page(perPage: $perPage) {
    media(id_in: $idIn) {
      id
      format
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub struct AniListClient {
    http: Client,
    base_url: Url,
    access_token: Option<String>,
    cache: Arc<RwLock<HashMap<i64, MediaFormat>>>,
    cache_path: PathBuf,
}

impl AniListClient {
    pub fn new(
        http: Client,
        access_token: Option<String>,
        data_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let base_url = Url::parse(ANILIST_BASE_URL)?;
        let cache_path = data_path.join(CACHE_FILENAME);
        let cache = load_cache(&cache_path)?;

        debug!(
            entries = cache.len(),
            path = %cache_path.display(),
            "loaded AniList format cache"
        );

        Ok(Self {
            http,
            base_url,
            access_token,
            cache: Arc::new(RwLock::new(cache)),
            cache_path,
        })
    }

    pub fn is_authenticated(&self) -> bool {
        self.access_token.is_some()
    }

    pub async fn fetch_media(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, AniListMedia>, AniListError> {
        let mut result = HashMap::new();
        if ids.is_empty() {
            return Ok(result);
        }

        let mut unique = ids.to_vec();
        unique.sort_unstable();
        unique.dedup();

        let mut missing: Vec<i64> = Vec::new();
        {
            let guard = self.cache.read().await;
            for id in unique.iter().copied() {
                match guard.get(&id) {
                    Some(format) => {
                        result.insert(
                            id,
                            AniListMedia {
                                format: format.clone(),
                            },
                        );
                    }
                    None => missing.push(id),
                }
            }
        }

        if missing.is_empty() {
            trace!(ids = unique.len(), "all AniList media served from cache");
            return Ok(result);
        }

        trace!(
            cached = result.len(),
            missing = missing.len(),
            "fetching uncached AniList media"
        );

        let mut fetched: HashMap<i64, MediaFormat> = HashMap::new();

        for chunk in missing.chunks(MAX_IDS_PER_REQUEST) {
            let request = GraphqlRequest {
                query: MEDIA_QUERY,
                variables: GraphqlVariables {
                    id_in: chunk.to_vec(),
                    per_page: MAX_IDS_PER_REQUEST,
                },
            };

            let mut builder = self.http.post(self.base_url.clone()).json(&request);

            // Use the auth token if its available
            if let Some(token) = &self.access_token {
                builder = builder.bearer_auth(token);
            }

            let response = builder.send().await?.error_for_status()?;

            let payload: GraphqlResponse = response.json().await?;

            if let Some(errors) = payload.errors
                && !errors.is_empty()
            {
                return Err(AniListError::Graphql(
                    errors
                        .into_iter()
                        .map(|err| err.message)
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
            }

            let data = payload.data.ok_or(AniListError::MissingData)?;
            let page = data.page.ok_or(AniListError::MissingData)?;

            let matches = page.media.len();
            for media in page.media.into_iter() {
                let Some(raw) = media.format.as_deref() else {
                    continue;
                };
                let Ok(format) = MediaFormat::from_str(raw) else {
                    continue;
                };

                fetched.entry(media.id).or_insert(format);
            }

            trace!(ids = chunk.len(), matches, "fetched AniList media batch");
        }

        if !fetched.is_empty() {
            for (id, format) in &fetched {
                result.insert(
                    *id,
                    AniListMedia {
                        format: format.clone(),
                    },
                );
            }

            {
                let mut guard = self.cache.write().await;
                guard.extend(fetched);
            }

            if let Err(error) = self.persist_cache().await {
                warn!(%error, "failed to save AniList cache to disk");
            }
        }

        Ok(result)
    }

    async fn persist_cache(&self) -> Result<(), AniListError> {
        // Clone snapshot under the read lock, then offload serialization + write
        // to a blocking thread to avoid blocking tokio worker threads.
        let snapshot = {
            let guard = self.cache.read().await;
            guard.clone()
        };

        let path = self.cache_path.clone();
        let write_err = |source: std::io::Error| AniListError::CacheWrite {
            source,
            path: self.cache_path.clone(),
        };

        task::spawn_blocking(move || -> std::io::Result<()> {
            let json = serde_json::to_vec_pretty(&snapshot)?;

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            std::fs::write(&path, json)
        })
        .await
        .map_err(|source| write_err(std::io::Error::other(format!("join error: {source}"))))?
        .map_err(write_err)
    }
}

fn load_cache(path: &Path) -> Result<HashMap<i64, MediaFormat>, AniListError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => {
            return Err(AniListError::CacheRead {
                source,
                path: path.to_path_buf(),
            });
        }
    };

    if bytes.is_empty() {
        return Ok(HashMap::new());
    }

    serde_json::from_slice(&bytes).map_err(|source| AniListError::CacheParse {
        source,
        path: path.to_path_buf(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, EnumString, Serialize, Deserialize)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MediaFormat {
    Tv,
    TvShort,
    Ona,

    Movie,
    Special,
    Ova,

    Music,
    Manga,
    Novel,
    OneShot,
}

#[derive(Debug, Clone)]
pub struct AniListMedia {
    pub format: MediaFormat,
}

#[derive(Debug, Serialize)]
struct GraphqlRequest {
    query: &'static str,
    variables: GraphqlVariables,
}

#[derive(Debug, Serialize)]
struct GraphqlVariables {
    #[serde(rename = "idIn")]
    id_in: Vec<i64>,
    #[serde(rename = "perPage")]
    per_page: usize,
}

#[derive(Debug, Deserialize)]
struct GraphqlResponse {
    data: Option<GraphqlData>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlData {
    #[serde(rename = "Page")]
    page: Option<GraphqlPage>,
}

#[derive(Debug, Deserialize)]
struct GraphqlPage {
    #[serde(default)]
    media: Vec<GraphqlMedia>,
}

#[derive(Debug, Deserialize)]
struct GraphqlMedia {
    id: i64,
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Debug, Error)]
pub enum AniListError {
    #[error("http error when querying AniList GraphQL API: {0}")]
    Http(#[from] reqwest::Error),
    #[error("failed to deserialise AniList response payload: {0}")]
    Deserialisation(#[from] serde_json::Error),
    #[error("AniList response missing data node")]
    MissingData,
    #[error("AniList GraphQL error(s): {0}")]
    Graphql(String),
    #[error("failed to read cached AniList formats at {path}")]
    CacheRead {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to write cached AniList formats at {path}")]
    CacheWrite {
        #[source]
        source: std::io::Error,
        path: PathBuf,
    },
    #[error("failed to parse cached AniList formats at {path}")]
    CacheParse {
        #[source]
        source: serde_json::Error,
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(dir: &Path) -> AniListClient {
        AniListClient::new(Client::new(), None, dir.to_path_buf()).unwrap()
    }

    #[test]
    fn load_cache_handles_missing_empty_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();

        assert!(
            load_cache(&dir.path().join("missing.json"))
                .unwrap()
                .is_empty()
        );

        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "").unwrap();
        assert!(load_cache(&empty).unwrap().is_empty());

        let populated = dir.path().join("populated.json");
        std::fs::write(&populated, r#"{"1":"TV","2":"MOVIE"}"#).unwrap();
        let cache = load_cache(&populated).unwrap();
        assert_eq!(cache.get(&1), Some(&MediaFormat::Tv));
        assert_eq!(cache.get(&2), Some(&MediaFormat::Movie));

        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(matches!(
            load_cache(&bad),
            Err(AniListError::CacheParse { .. })
        ));
    }

    #[tokio::test]
    async fn cached_ids_are_served_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILENAME), r#"{"1":"TV","2":"OVA"}"#).unwrap();

        // No network access is possible for these ids: a request would surface
        // as an error, so a successful result proves the cache was used.
        let client = client(dir.path());
        let media = client.fetch_media(&[2, 1, 1]).await.unwrap();

        assert_eq!(media.len(), 2);
        assert_eq!(media[&1].format, MediaFormat::Tv);
        assert_eq!(media[&2].format, MediaFormat::Ova);
    }

    #[tokio::test]
    async fn persist_cache_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let client = client(&dir.path().join("nested"));

        client.cache.write().await.insert(7, MediaFormat::Special);
        client.persist_cache().await.unwrap();

        let reloaded = load_cache(&client.cache_path).unwrap();
        assert_eq!(reloaded.get(&7), Some(&MediaFormat::Special));
    }
}
