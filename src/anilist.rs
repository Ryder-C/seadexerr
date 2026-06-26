use std::{collections::HashMap, str::FromStr};

use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use strum::EnumString;
use thiserror::Error;
use tracing::trace;

const ANILIST_BASE_URL: &str = "https://graphql.anilist.co";
const MAX_IDS_PER_REQUEST: usize = 50;

const MEDIA_QUERY: &str = r#"
query MediaById($idIn: [Int], $perPage: Int) {
  Page(perPage: $perPage) {
    media(id_in: $idIn) {
      id
      type
      format
    }
  }
}
"#;

#[derive(Debug, Clone)]
pub struct AniListClient {
    http: Client,
    base_url: Url,
}

impl AniListClient {
    pub fn new(http: Client) -> anyhow::Result<Self> {
        let base_url = Url::parse(ANILIST_BASE_URL)?;

        Ok(Self { http, base_url })
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

        for chunk in unique.chunks(MAX_IDS_PER_REQUEST.max(1)) {
            let request = GraphqlRequest {
                query: MEDIA_QUERY,
                variables: GraphqlVariables {
                    id_in: chunk.to_vec(),
                    per_page: MAX_IDS_PER_REQUEST,
                },
            };

            let response = self
                .http
                .post(self.base_url.clone())
                .json(&request)
                .send()
                .await?
                .error_for_status()?;

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

                result.entry(media.id).or_insert(AniListMedia { format });
            }

            trace!(ids = chunk.len(), matches, "fetched AniList media batch");
        }

        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, EnumString)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
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
    #[serde(rename = "type")]
    _media_type: Option<String>,
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
}
