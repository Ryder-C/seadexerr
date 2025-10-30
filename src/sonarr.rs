use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tracing::debug;
use url::Url;

#[derive(Debug, Clone)]
pub struct SonarrClient {
    http: Client,
    base_url: Url,
    api_key: String,
}

impl SonarrClient {
    pub fn new(base_url: Url, api_key: String, timeout: Duration) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            http,
            base_url,
            api_key,
        })
    }

    pub async fn resolve_name(&self, tvdb_id: i64) -> Result<String, SonarrError> {
        let mut url = self
            .base_url
            .join("api/v3/series/lookup")
            .map_err(SonarrError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("term", &format!("tvdb:{tvdb_id}"));
        }

        debug!(
            tvdb_id,
            url = %url,
            "requesting Sonarr series lookup"
        );

        let response = self
            .http
            .get(url)
            .header("X-Api-Key", &self.api_key)
            .send()
            .await?
            .error_for_status()?;

        let payload: Vec<SeriesLookupEntry> = response.json().await?;

        debug!(
            tvdb_id,
            results = payload.len(),
            "Sonarr series lookup response received"
        );

        let Some(title) = payload.into_iter().find_map(|entry| entry.title) else {
            return Err(SonarrError::NotFound { tvdb_id });
        };

        Ok(title)
    }
}

#[derive(Debug, Deserialize)]
struct SeriesLookupEntry {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Error)]
pub enum SonarrError {
    #[error("failed to build Sonarr request url")]
    Url(#[from] url::ParseError),
    #[error("http error when querying Sonarr api")]
    Http(#[from] reqwest::Error),
    #[error("no Sonarr series title found for tvdb {tvdb_id}")]
    NotFound { tvdb_id: i64 },
}
