use std::collections::HashMap;
use std::time::Duration;

use reqwest::{Client, Url};
use serde::Deserialize;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct PlexAniBridgeClient {
    http: Client,
    base_url: Url,
    per_request_cap: usize,
}

impl PlexAniBridgeClient {
    pub fn new(base_url: Url, timeout: Duration, per_request_cap: usize) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()?;

        let per_request_cap = per_request_cap.max(50);

        Ok(Self {
            http,
            base_url,
            per_request_cap,
        })
    }

    pub async fn resolve_anilist_id(
        &self,
        tvdb_id: i64,
        season: u32,
    ) -> Result<Option<i64>, MappingError> {
        let mut url = self
            .base_url
            .join("api/v2/search")
            .map_err(MappingError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("tvdb_id", &tvdb_id.to_string());
            pairs.append_pair("page", "1");
            pairs.append_pair("limit", &self.per_request_cap.to_string());
        }

        debug!(
            tvdb_id,
            season,
            limit = self.per_request_cap,
            "requesting plexanibridge mappings"
        );

        let response = self.http.get(url).send().await?.error_for_status()?;
        let payload: SearchResponse = response.json().await?;

        let key = format!("s{season}");
        debug!(
            tvdb_id,
            season,
            candidates = payload.results.len(),
            "plexanibridge response received"
        );

        for record in payload.results {
            let Some(anilist_id) = record.anilist_id else {
                continue;
            };

            let Some(mappings) = record.tvdb_mappings else {
                continue;
            };

            if mappings.contains_key(&key) {
                debug!(
                    tvdb_id,
                    season, anilist_id, "matched mapping entry for season"
                );
                return Ok(Some(anilist_id));
            }
        }

        debug!(
            tvdb_id,
            season, "no season-specific mapping found in plexanibridge response"
        );

        Ok(None)
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Record>,
}

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    anilist_id: Option<i64>,
    #[serde(default)]
    tvdb_mappings: Option<HashMap<String, String>>,
}

#[derive(Debug, Error)]
pub enum MappingError {
    #[error("failed to build plexanibridge request url")]
    Url(#[from] url::ParseError),
    #[error("http error when querying plexanibridge api")]
    Http(#[from] reqwest::Error),
    #[error("failed to deserialise plexanibridge response payload")]
    Deserialisation(#[from] serde_json::Error),
}
