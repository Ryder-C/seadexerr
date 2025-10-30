use std::time::Duration;

use reqwest::{Client, Url};
use serde::Deserialize;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::debug;

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
            .user_agent(format!("seadexerr/{}", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            http,
            base_url,
            default_limit,
        })
    }

    pub async fn search_torrents(
        &self,
        anilist_id: i64,
        limit: usize,
    ) -> Result<Vec<Torrent>, ReleasesError> {
        let mut url = self
            .base_url
            .join("collections/entries/records")
            .map_err(ReleasesError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("filter", &format!("(alID={anilist_id})"));
            pairs.append_pair("expand", "trs");
            pairs.append_pair("page", "1");
            pairs.append_pair("perPage", &limit.min(self.default_limit).to_string());
        }

        let response = self.http.get(url).send().await?.error_for_status()?;
        let payload: EntriesResponse = response.json().await?;

        debug!(
            anilist_id,
            limit,
            items = payload.items.len(),
            "releases.moe entries response received"
        );

        let torrents: Vec<Torrent> = payload
            .items
            .into_iter()
            .filter_map(|entry| entry.expand)
            .flat_map(|expand| expand.trs.into_iter())
            .filter(|record| rewritten_download_url(record).is_some())
            .map(Torrent::from)
            .take(limit)
            .collect();

        debug!(
            anilist_id,
            total = torrents.len(),
            "constructed torrent results from releases.moe entries"
        );

        Ok(torrents)
    }

    pub async fn recent_public_torrents(
        &self,
        limit: usize,
    ) -> Result<Vec<Torrent>, ReleasesError> {
        let mut url = self
            .base_url
            .join("collections/torrents/records")
            .map_err(ReleasesError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("filter", "(tracker='Nyaa')");
            pairs.append_pair("sort", "-updated");
            pairs.append_pair("page", "1");
            pairs.append_pair("perPage", &limit.min(self.default_limit).to_string());
        }

        let response = self.http.get(url).send().await?.error_for_status()?;
        let payload: TorrentsResponse = response.json().await?;

        debug!(
            feed = "recent-public",
            limit,
            returned = payload.items.len(),
            "releases.moe torrent list response received"
        );

        Ok(payload.items.into_iter().map(Torrent::from).collect())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct EntriesResponse {
    items: Vec<EntryRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct EntryRecord {
    #[serde(default)]
    expand: Option<EntryExpand>,
}

#[derive(Debug, Clone, Deserialize)]
struct EntryExpand {
    #[serde(default)]
    trs: Vec<TorrentRecord>,
}

#[derive(Debug, Clone)]
pub struct Torrent {
    pub id: String,
    pub download_url: String,
    pub info_hash: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub files: Vec<TorrentFile>,
    pub size_bytes: u64,
    pub is_best: bool,
}

impl From<TorrentRecord> for Torrent {
    fn from(record: TorrentRecord) -> Self {
        let download_url = rewritten_download_url(&record).unwrap_or_else(|| record.url.clone());

        let size_bytes = record.files.iter().map(|f| f.length).sum::<u64>();
        Torrent {
            id: record.id,
            download_url,
            info_hash: record.info_hash,
            published: record
                .updated
                .as_deref()
                .and_then(parse_timestamp)
                .or_else(|| record.created.as_deref().and_then(parse_timestamp)),
            files: record.files,
            size_bytes,
            is_best: record.is_best,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentRecord {
    id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    #[serde(rename = "infoHash")]
    info_hash: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
    #[serde(rename = "isBest")]
    is_best: bool,
    files: Vec<TorrentFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TorrentFile {
    pub length: u64,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentsResponse {
    items: Vec<TorrentRecord>,
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(parsed);
    }

    let mut normalized = value.replace(' ', "T");
    if !normalized.ends_with('Z') {
        normalized.push('Z');
    }

    OffsetDateTime::parse(&normalized, &Rfc3339).ok()
}

fn rewritten_download_url(record: &TorrentRecord) -> Option<String> {
    extract_nyaa_id(record.url.as_str()).map(|id| format!("https://nyaa.si/download/{id}.torrent"))
}

fn extract_nyaa_id(url: &str) -> Option<&str> {
    let needle = "/view/";
    let start = url.find(needle)? + needle.len();
    let rest = &url[start..];
    let id = rest
        .split(|c: char| c == '?' || c == '#' || c == '/')
        .next()
        .unwrap_or("");
    if id.is_empty() || !id.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(id)
}

#[derive(Debug, Error)]
pub enum ReleasesError {
    #[error("failed to build releases.moe request url")]
    Url(#[from] url::ParseError),
    #[error("HTTP error when querying releases.moe")]
    Http(#[from] reqwest::Error),
    #[error("failed to deserialise releases.moe response payload")]
    Deserialisation(#[from] serde_json::Error),
}
