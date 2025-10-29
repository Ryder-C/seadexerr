use std::{collections::HashMap, time::Duration};

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
            .filter(|record| {
                record
                    .tracker
                    .as_deref()
                    .map(|tracker| tracker.eq_ignore_ascii_case("nyaa"))
                    .unwrap_or(false)
            })
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

impl From<TorrentRecord> for Torrent {
    fn from(record: TorrentRecord) -> Self {
        let original_url = record.url.clone();
        let download_url = rewritten_download_url(&record).unwrap_or_else(|| original_url.clone());

        let size_total = record
            .files
            .iter()
            .fold(0u64, |acc, file| acc.saturating_add(file.length));
        let size_bytes = if size_total > 0 {
            Some(size_total)
        } else {
            None
        };

        let title = record.title().unwrap_or_else(|| record.id.clone());

        Torrent {
            id: record.id.clone(),
            title,
            download_url,
            magnet_uri: record
                .info_hash
                .as_ref()
                .map(|hash| format!("magnet:?xt=urn:btih:{}", hash)),
            size_bytes,
            seeders: None,
            leechers: None,
            info_hash: record.info_hash,
            published: record
                .updated
                .as_deref()
                .and_then(parse_timestamp)
                .or_else(|| record.created.as_deref().and_then(parse_timestamp)),
            release_group: record.release_group,
            tracker: record.tracker,
            comments: record
                .grouped_url
                .and_then(|url| (!url.trim().is_empty()).then_some(url))
                .or_else(|| (!original_url.trim().is_empty()).then_some(original_url)),
            description: record
                .tags
                .as_ref()
                .filter(|tags| !tags.is_empty())
                .map(|tags| tags.join(", ")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentRecord {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    #[serde(rename = "releaseGroup")]
    release_group: Option<String>,
    #[serde(default)]
    #[serde(rename = "tracker")]
    tracker: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    #[serde(rename = "groupedUrl")]
    grouped_url: Option<String>,
    #[serde(default)]
    #[serde(rename = "infoHash")]
    info_hash: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    files: Vec<TorrentFileRecord>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    updated: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentFileRecord {
    length: u64,
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

impl TorrentRecord {
    fn title(&self) -> Option<String> {
        if let Some(title) = self.title.as_ref() {
            if !title.trim().is_empty() {
                return Some(title.trim().to_string());
            }
        }

        if let Some(name) = self.name.as_ref() {
            if !name.trim().is_empty() {
                return Some(name.trim().to_string());
            }
        }

        if let Some(series_title) = self.episode_range_title() {
            return Some(series_title);
        }

        self.files
            .iter()
            .find_map(|file| {
                let trimmed = file.name.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .or_else(|| self.release_group.clone())
    }

    fn episode_range_title(&self) -> Option<String> {
        let mut range_by_prefix: HashMap<String, (u32, u32, usize, usize)> = HashMap::new();

        for file in &self.files {
            let name = file.name.trim();
            if name.is_empty() {
                continue;
            }

            let (prefix, suffix) = match name.rsplit_once(" - ") {
                Some(split) => split,
                None => continue,
            };

            let token = suffix.split_whitespace().next().unwrap_or("");
            let digits: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                continue;
            }

            let episode = match digits.parse::<u32>() {
                Ok(value) => value,
                Err(_) => continue,
            };

            let prefix = prefix.trim().to_string();
            let entry =
                range_by_prefix
                    .entry(prefix)
                    .or_insert((episode, episode, digits.len(), 0));
            entry.0 = entry.0.min(episode);
            entry.1 = entry.1.max(episode);
            entry.2 = entry.2.max(digits.len());
            entry.3 += 1;
        }

        let (prefix, (min_ep, max_ep, width, count)) = range_by_prefix
            .into_iter()
            .max_by(|a, b| compare_episode_ranges(a, b))?;

        if count < 2 {
            return None;
        }

        let format_ep = |value: u32| format!("{value:0width$}", width = width);
        if min_ep == max_ep {
            Some(format!("{} - {}", prefix, format_ep(min_ep)))
        } else {
            Some(format!(
                "{} - {} ~ {}",
                prefix,
                format_ep(min_ep),
                format_ep(max_ep)
            ))
        }
    }
}

fn compare_episode_ranges(
    a: &(String, (u32, u32, usize, usize)),
    b: &(String, (u32, u32, usize, usize)),
) -> std::cmp::Ordering {
    let (_, (min_a, max_a, _, count_a)) = a;
    let (_, (min_b, max_b, _, count_b)) = b;

    let range_a = (*max_a).saturating_sub(*min_a);
    let range_b = (*max_b).saturating_sub(*min_b);

    range_a
        .cmp(&range_b)
        .then_with(|| count_a.cmp(count_b))
        .then_with(|| max_a.cmp(max_b))
}

fn rewritten_download_url(record: &TorrentRecord) -> Option<String> {
    let tracker = record.tracker.as_deref()?;
    if !tracker.eq_ignore_ascii_case("nyaa") {
        return None;
    }

    let url = record.url.as_str();
    extract_nyaa_id(url).map(|id| format!("https://nyaa.si/download/{id}.torrent"))
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
