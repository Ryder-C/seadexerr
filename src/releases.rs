use std::collections::{HashMap, HashSet};

use anyhow::Result;
use reqwest::{Client, Url};
use serde::Deserialize;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::trace;

const RELEASES_BASE_URL: &str = "https://releases.moe/api/";
const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone)]
pub struct ReleasesClient {
    http: Client,
    base_url: Url,
    ab_passkey: Option<String>,
}

impl ReleasesClient {
    pub fn new(http: Client, ab_passkey: Option<&str>) -> Result<Self> {
        let base_url = Url::parse(RELEASES_BASE_URL)?;

        let ab_passkey = ab_passkey.map(|k| k.to_string());
        Ok(Self {
            http,
            base_url,
            ab_passkey,
        })
    }

    pub async fn search_torrents(&self, anilist_id: i64) -> Result<Vec<Torrent>, ReleasesError> {
        let payload = self
            .fetch_entries_with(|params| {
                params.push(("filter".to_string(), format!("alID={anilist_id}")));
            })
            .await?;

        let torrents = Self::entries_to_torrents(payload.items, self.ab_passkey.as_deref());

        trace!(
            anilist_id,
            total = torrents.len(),
            "constructed torrent results from releases.moe entries"
        );

        Ok(torrents)
    }

    pub async fn recent_public_torrents(&self) -> Result<Vec<Torrent>, ReleasesError> {
        let payload = self
            .fetch_entries_with(|params| {
                params.push(("sort".to_string(), "-updated".to_string()));
            })
            .await?;

        let torrents = Self::entries_to_torrents(payload.items, self.ab_passkey.as_deref());

        trace!(
            feed = "recent-public",
            returned = torrents.len(),
            "releases.moe entries response received"
        );

        Ok(torrents)
    }

    async fn fetch_entries_with<F>(&self, configure: F) -> Result<EntriesResponse, ReleasesError>
    where
        F: FnOnce(&mut Vec<(String, String)>),
    {
        let mut params = vec![
            ("expand".to_string(), "trs".to_string()),
            ("page".to_string(), "1".to_string()),
            ("perPage".to_string(), PAGE_SIZE.to_string()),
        ];
        configure(&mut params);

        let mut url = self
            .base_url
            .join("collections/entries/records")
            .map_err(ReleasesError::Url)?;

        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(&key, &value);
            }
        }

        let response = self.http.get(url).send().await?.error_for_status()?;
        let payload: EntriesResponse = response.json().await?;

        Ok(payload)
    }

    fn entries_to_torrents(entries: Vec<EntryRecord>, ab_passkey: Option<&str>) -> Vec<Torrent> {
        entries
            .into_iter()
            .flat_map(|entry| {
                let al_id = entry.al_id;
                entry.expand.into_iter().flat_map(move |expand| {
                    expand.trs.into_iter().map(move |record| (al_id, record))
                })
            })
            .filter_map(|(al_id, record)| {
                let tracker = Tracker::from_name(&record.tracker)?;
                if !tracker.is_enabled(ab_passkey) {
                    return None;
                }
                if record.tags.contains(&"Incomplete".to_string()) {
                    return None;
                }
                let download_url = tracker.download_url(&record.url, ab_passkey)?;
                Some(Torrent::from_record(record, al_id, tracker, download_url))
            })
            .collect()
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
        let mut unique_ids: Vec<String> = unique.into_iter().collect();
        unique_ids.sort_unstable();

        const CHUNK_SIZE: usize = 20;

        for chunk in unique_ids.chunks(CHUNK_SIZE) {
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
                pairs.append_pair("perPage", &PAGE_SIZE.to_string());
            }

            let response = self.http.get(url).send().await?.error_for_status()?;
            let payload: EntriesResponse = response.json().await?;

            let requested: HashSet<&str> = chunk.iter().map(|id| id.as_str()).collect();

            for entry in payload.items {
                let Some(expand) = entry.expand else { continue };
                let Some(al_id) = entry.al_id else { continue };

                for record in expand.trs {
                    if Tracker::from_name(&record.tracker).is_none() {
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
    al_id: Option<i64>,
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
    pub dual_audio: bool,
    pub tags: Vec<String>,
    pub anilist_id: Option<i64>,
    pub tracker: Tracker,
}

impl Torrent {
    fn from_record(
        record: TorrentRecord,
        anilist_id: Option<i64>,
        tracker: Tracker,
        download_url: String,
    ) -> Self {
        let source_url = tracker.absolute_source_url(&record.url);
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
            dual_audio: record.dual_audio,
            tags: record.tags,
            anilist_id,
            source_url,
            tracker,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TorrentRecord {
    id: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "infoHash")]
    info_hash: Option<String>,
    created: Option<String>,
    updated: Option<String>,
    #[serde(rename = "isBest")]
    is_best: bool,
    #[serde(rename = "dualAudio", default)]
    dual_audio: bool,
    tags: Vec<String>,
    #[serde(default)]
    tracker: String,
    files: Vec<TorrentFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TorrentFile {
    pub length: u64,
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

/// A torrent tracker we know how to construct download/source URLs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracker {
    Nyaa,
    AnimeBytes,
}

impl Tracker {
    /// Maps a releases.moe `tracker` value to a known tracker, or `None` if unsupported.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "Nyaa" => Some(Tracker::Nyaa),
            "AB" => Some(Tracker::AnimeBytes),
            _ => None,
        }
    }

    /// Whether this tracker can be used given the current configuration.
    fn is_enabled(self, ab_passkey: Option<&str>) -> bool {
        match self {
            Tracker::Nyaa => true,
            Tracker::AnimeBytes => ab_passkey.is_some(),
        }
    }

    /// Builds the `.torrent` download URL for a record, or `None` if the record's
    /// source URL doesn't contain a usable id (or the passkey is missing for AB).
    fn download_url(self, record_url: &str, ab_passkey: Option<&str>) -> Option<String> {
        match self {
            Tracker::Nyaa => {
                let id = extract_id(record_url, "/view/")?;
                Some(format!("https://nyaa.si/download/{id}.torrent"))
            }
            Tracker::AnimeBytes => {
                let passkey = ab_passkey?;
                let torrentid = extract_id(record_url, "torrentid=")?;
                Some(format!(
                    "https://animebytes.tv/torrent/{torrentid}/download/{passkey}"
                ))
            }
        }
    }

    /// Resolves a record's (possibly relative) source URL to an absolute one.
    fn absolute_source_url(self, record_url: &str) -> String {
        match self {
            Tracker::AnimeBytes if record_url.starts_with('/') => {
                format!("https://animebytes.tv{record_url}")
            }
            _ => record_url.to_string(),
        }
    }
}

fn extract_id<'a>(url: &'a str, needle: &str) -> Option<&'a str> {
    let start = url.find(needle)? + needle.len();
    let rest = &url[start..];
    let id = rest.split(['?', '#', '/', '&']).next().unwrap_or("");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_torrent_record(id: &str) -> TorrentRecord {
        TorrentRecord {
            id: id.to_string(),
            url: format!("https://nyaa.si/view/{id}"),
            info_hash: None,
            created: None,
            updated: None,
            is_best: false,
            dual_audio: false,
            tags: Vec::new(),
            tracker: "Nyaa".to_string(),
            files: Vec::new(),
        }
    }

    fn make_ab_torrent_record(id: &str, torrentid: &str) -> TorrentRecord {
        TorrentRecord {
            id: id.to_string(),
            url: format!("/torrents.php?id={}&torrentid={}", id, torrentid),
            info_hash: None,
            created: None,
            updated: None,
            is_best: false,
            dual_audio: false,
            tags: Vec::new(),
            tracker: "AB".to_string(),
            files: Vec::new(),
        }
    }

    fn make_file(length: u64) -> TorrentFile {
        TorrentFile { length }
    }

    fn make_entry(al_id: Option<i64>, trs: Vec<TorrentRecord>) -> EntryRecord {
        EntryRecord {
            al_id,
            expand: Some(EntryExpand { trs }),
        }
    }

    #[test]
    fn parses_releases_base_url() {
        ReleasesClient::new(crate::http::client().unwrap(), None)
            .expect("client construction must succeed");
    }

    #[test]
    fn extracts_nyaa_id() {
        assert_eq!(
            extract_id("https://nyaa.si/view/12345", "/view/"),
            Some("12345")
        );
        assert_eq!(
            extract_id("https://nyaa.si/view/12345?foo=bar", "/view/"),
            Some("12345")
        );
        assert_eq!(
            extract_id("https://nyaa.si/view/12345#section", "/view/"),
            Some("12345")
        );
        assert_eq!(
            extract_id("https://nyaa.si/view/12345/extra", "/view/"),
            Some("12345")
        );
        assert_eq!(extract_id("https://nyaa.si/view/abc", "/view/"), None);
        assert_eq!(extract_id("https://nyaa.si/view/12a45", "/view/"), None);
        assert_eq!(
            extract_id("https://nyaa.si/something/12345", "/view/"),
            None
        );
        assert_eq!(extract_id("", "/view/"), None);
    }

    #[test]
    fn extracts_ab_torrent_id() {
        assert_eq!(
            extract_id("/torrents.php?id=70543&torrentid=1143533", "torrentid="),
            Some("1143533")
        );
        assert_eq!(
            extract_id(
                "/torrents.php?id=70543&torrentid=1143533&extra=1",
                "torrentid="
            ),
            Some("1143533")
        );
        assert_eq!(
            extract_id(
                "/torrents.php?id=70543&torrentid=1143533#section",
                "torrentid="
            ),
            Some("1143533")
        );
        assert_eq!(extract_id("/torrents.php?id=70543", "torrentid="), None);
        assert_eq!(extract_id("", "torrentid="), None);
    }

    #[test]
    fn parses_timestamp() {
        let parsed = parse_timestamp("2024-01-02T03:04:05Z").expect("rfc3339 must parse");
        assert_eq!(parsed.year(), 2024);

        let parsed = parse_timestamp("2024-01-02 03:04:05").expect("space-separated must parse");
        let expected = OffsetDateTime::parse("2024-01-02T03:04:05Z", &Rfc3339).unwrap();
        assert_eq!(parsed, expected);

        assert!(parse_timestamp("not a timestamp").is_none());
    }

    #[test]
    fn builds_torrent_from_record() {
        let with_files = TorrentRecord {
            files: vec![make_file(100), make_file(200), make_file(300)],
            ..make_torrent_record("1")
        };
        let download_url = "https://nyaa.si/download/1.torrent".to_string();
        assert_eq!(
            Torrent::from_record(with_files, None, Tracker::Nyaa, download_url).size_bytes,
            600
        );

        let both_timestamps = TorrentRecord {
            created: Some("2024-01-01T00:00:00Z".to_string()),
            updated: Some("2024-02-02T00:00:00Z".to_string()),
            ..make_torrent_record("1")
        };
        let updated = OffsetDateTime::parse("2024-02-02T00:00:00Z", &Rfc3339).unwrap();
        let download_url = "https://nyaa.si/download/1.torrent".to_string();
        assert_eq!(
            Torrent::from_record(both_timestamps, None, Tracker::Nyaa, download_url).published,
            Some(updated)
        );

        let only_created = TorrentRecord {
            created: Some("2024-01-01T00:00:00Z".to_string()),
            updated: None,
            ..make_torrent_record("1")
        };
        let created = OffsetDateTime::parse("2024-01-01T00:00:00Z", &Rfc3339).unwrap();
        let download_url = "https://nyaa.si/download/1.torrent".to_string();
        assert_eq!(
            Torrent::from_record(only_created, None, Tracker::Nyaa, download_url).published,
            Some(created)
        );

        let with_nyaa_url = TorrentRecord {
            url: "https://nyaa.si/view/9876".to_string(),
            ..make_torrent_record("1")
        };
        let download_url = "https://nyaa.si/download/9876.torrent".to_string();
        let torrent =
            Torrent::from_record(with_nyaa_url, None, Tracker::Nyaa, download_url.clone());
        assert_eq!(torrent.download_url, download_url);
        assert_eq!(torrent.source_url, "https://nyaa.si/view/9876");
        assert_eq!(torrent.tracker, Tracker::Nyaa);

        let with_ab_url = make_ab_torrent_record("70543", "1143533");
        let ab_download_url = "https://animebytes.tv/torrent/1143533/download/test".to_string();
        let torrent = Torrent::from_record(with_ab_url, None, Tracker::AnimeBytes, ab_download_url);
        assert_eq!(torrent.tracker, Tracker::AnimeBytes);
        assert_eq!(
            torrent.source_url,
            "https://animebytes.tv/torrents.php?id=70543&torrentid=1143533"
        );
    }

    #[test]
    fn filters_entries_to_torrents() {
        let with_other_tracker = vec![make_entry(
            Some(42),
            vec![
                make_torrent_record("1"),
                TorrentRecord {
                    tracker: "AnimeBytes".to_string(),
                    ..make_torrent_record("2")
                },
            ],
        )];
        let torrents = ReleasesClient::entries_to_torrents(with_other_tracker, None);
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].id, "1");

        let with_incomplete_tag = vec![make_entry(
            Some(42),
            vec![
                make_torrent_record("1"),
                TorrentRecord {
                    tags: vec!["Incomplete".to_string()],
                    ..make_torrent_record("2")
                },
            ],
        )];
        let torrents = ReleasesClient::entries_to_torrents(with_incomplete_tag, None);
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].id, "1");

        let with_bad_url = vec![make_entry(
            Some(42),
            vec![
                make_torrent_record("1"),
                TorrentRecord {
                    url: "https://example.com/not-a-nyaa-link".to_string(),
                    ..make_torrent_record("2")
                },
            ],
        )];
        let torrents = ReleasesClient::entries_to_torrents(with_bad_url, None);
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].id, "1");

        let single = vec![make_entry(Some(12345), vec![make_torrent_record("1")])];
        let torrents = ReleasesClient::entries_to_torrents(single, None);
        assert_eq!(torrents[0].anilist_id, Some(12345));
    }

    #[test]
    fn ab_entries_included_with_passkey() {
        let entries = vec![make_entry(
            Some(42),
            vec![
                make_torrent_record("1"),
                make_ab_torrent_record("70543", "1143533"),
            ],
        )];
        let torrents = ReleasesClient::entries_to_torrents(entries, Some("testkey"));
        assert_eq!(torrents.len(), 2);
        assert_eq!(torrents[0].tracker, Tracker::Nyaa);
        assert_eq!(torrents[1].tracker, Tracker::AnimeBytes);
        assert!(
            torrents[1]
                .download_url
                .contains("animebytes.tv/torrent/1143533/download/testkey")
        );
    }

    #[test]
    fn ab_entries_excluded_without_passkey() {
        let entries = vec![make_entry(
            Some(42),
            vec![
                make_torrent_record("1"),
                make_ab_torrent_record("70543", "1143533"),
            ],
        )];
        let torrents = ReleasesClient::entries_to_torrents(entries, None);
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].tracker, Tracker::Nyaa);
    }
}
