use std::time::Duration;

use reqwest::{Client, Url};
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct ReleasesClient {
    http: Client,
    base_url: Url,
    default_limit: usize,
}

impl ReleasesClient {
    pub fn new(base_url: Url, timeout: Duration, default_limit: usize) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(timeout)
            .user_agent(format!("seadexer/{}", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            http,
            base_url,
            default_limit,
        })
    }

    pub async fn search_torrents(
        &self,
        request: TorrentSearchRequest,
    ) -> Result<Vec<Torrent>, ReleasesError> {
        let _ = (&self.http, &self.base_url, self.default_limit);
        let TorrentSearchRequest {
            query,
            limit,
            offset,
        } = request;
        let _ = (query, limit, offset);
        // TODO: Wire up the releases.moe request + response mapping once the exact API contract is finalised.
        Err(ReleasesError::NotImplemented)
    }
}

#[derive(Debug, Clone)]
pub struct TorrentSearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Torrent {
    pub id: String,
    pub title: String,
    pub download_url: String,
    pub magnet_uri: Option<String>,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub info_hash: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub release_group: Option<String>,
    pub tracker: Option<String>,
    pub comments: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Error)]
pub enum ReleasesError {
    #[error("failed to build releases.moe request url")]
    InvalidUrl(#[from] url::ParseError),
    #[error("HTTP error when querying releases.moe")]
    Http(#[from] reqwest::Error),
    #[error("failed to deserialise releases.moe response payload")]
    Deserialisation(#[from] serde_json::Error),
    #[error("releases.moe client functionality has not been implemented yet")]
    NotImplemented,
}
