use std::collections::{HashMap, HashSet};
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
            .flat_map(|entry| {
                let al_id = entry.al_id;
                entry.expand.into_iter().flat_map(move |expand| {
                    expand.trs.into_iter().map(move |record| (al_id, record))
                })
            })
            .filter(|(_, record)| rewritten_download_url(record).is_some())
            .filter(|(_, record)| record.tracker == "Nyaa")
            .map(|(al_id, record)| Torrent::from_record(record, al_id))
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

        Ok(payload
            .items
            .into_iter()
            .map(|record| Torrent::from_record(record, None))
            .collect())
    }

    pub async fn resolve_anilist_ids_for_torrents(
        &self,
        torrent_ids: &[String],
    ) -> Result<HashMap<String, i64>, ReleasesError> {
        let mut result = HashMap::new();
        if torrent_ids.is_empty() {
            return Ok(result);
        }

        let unique: HashSet<String> = torrent_ids.iter().cloned().collect();
        if unique.is_empty() {
            return Ok(result);
        }

        let mut unique_ids: Vec<String> = unique.into_iter().collect();
        unique_ids.sort_unstable();

        const CHUNK_SIZE: usize = 20;

        for chunk in unique_ids.chunks(CHUNK_SIZE.max(1)) {
            let filter = chunk
                .iter()
                .map(|id| format!("(trs~'{}')", id))
                .collect::<Vec<_>>()
                .join(" || ");

            if filter.is_empty() {
                continue;
            }

            let mut url = self
                .base_url
                .join("collections/entries/records")
                .map_err(ReleasesError::Url)?;

            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("filter", &filter);
                pairs.append_pair("expand", "trs");
                let per_page = std::cmp::max(self.default_limit, chunk.len());
                pairs.append_pair("perPage", &per_page.to_string());
            }

            let response = self.http.get(url).send().await?.error_for_status()?;
            let payload: EntriesResponse = response.json().await?;

            let requested: HashSet<&str> = chunk.iter().map(|id| id.as_str()).collect();

            for entry in payload.items {
                let Some(expand) = entry.expand else { continue };
                let Some(al_id) = entry.al_id else { continue };

                for record in expand.trs {
                    if record.tracker != "Nyaa" {
                        continue;
                    }

                    if requested.contains(record.id.as_str()) {
                        result.insert(record.id, al_id);
                    }
                }
            }
        }

        Ok(result)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct EntriesResponse {
    items: Vec<EntryRecord>,
}

#[derive(Debug, Clone, Deserialize)]
struct EntryRecord {
    #[serde(rename = "alID")]
    #[serde(default)]
    al_id: Option<i64>,
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
    pub source_url: String,
    pub info_hash: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub files: Vec<TorrentFile>,
    pub size_bytes: u64,
    pub is_best: bool,
    pub anilist_id: Option<i64>,
}

impl Torrent {
    fn from_record(record: TorrentRecord, anilist_id: Option<i64>) -> Self {
        let download_url = rewritten_download_url(&record).unwrap_or_else(|| record.url.clone());
        let source_url = record.url.clone();

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
            anilist_id,
            source_url,
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
    #[serde(default)]
    tracker: String,
    files: Vec<TorrentFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TorrentFile {
    pub length: u64,
    #[serde(rename = "name")]
    pub name: String,
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
    let id = rest.split(['?', '#', '/']).next().unwrap_or("");
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
