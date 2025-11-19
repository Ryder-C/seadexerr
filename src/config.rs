use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use reqwest::Url;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub listen_addr: SocketAddr,
    pub public_base_url: Option<Url>,
    pub releases_base_url: Url,
    pub releases_timeout: Duration,
    pub data_path: PathBuf,
    pub mapping_source_url: Url,
    pub mapping_refresh_interval: Duration,
    pub mapping_timeout: Duration,
    pub application_title: String,
    pub application_description: String,
    pub default_limit: usize,
    pub anilist_base_url: Url,
    pub anilist_timeout: Duration,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
}

#[derive(Clone, Debug)]
pub struct SonarrConfig {
    pub url: Url,
    pub api_key: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct RadarrConfig {
    pub url: Url,
    pub api_key: String,
    pub timeout: Duration,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let host = env::var("SEADEXER_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = env::var("SEADEXER_PORT").unwrap_or_else(|_| "6767".to_string());
        let port = port
            .parse::<u16>()
            .context("SEADEXER_PORT must be a valid u16 integer")?;
        let listen_addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .context("failed to parse socket address from SEADEXER_HOST and SEADEXER_PORT")?;

        let raw_base_url = env::var("SEADEXER_RELEASES_BASE_URL")
            .unwrap_or_else(|_| "https://releases.moe/api/".to_string());
        let releases_base_url = parse_root_url(&raw_base_url, "SEADEXER_RELEASES_BASE_URL")?;

        let data_path = env::var("SEADEXER_DATA_PATH").unwrap_or_else(|_| "data".to_string());
        let data_path = PathBuf::from(data_path);

        let raw_mapping_source_url = env::var("SEADEXER_MAPPING_SOURCE_URL").unwrap_or_else(|_| {
            "https://raw.githubusercontent.com/eliasbenb/PlexAniBridge-Mappings/refs/heads/v2/mappings.json".to_string()
        });
        let mapping_source_url = Url::parse(&raw_mapping_source_url)
            .context("SEADEXER_MAPPING_SOURCE_URL must be a valid URL")?;

        let mapping_refresh_secs = env::var("SEADEXER_MAPPING_REFRESH_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(21_600);
        let mapping_refresh_interval = Duration::from_secs(mapping_refresh_secs);

        let public_base_url = env::var("SEADEXER_PUBLIC_BASE_URL")
            .ok()
            .map(|value| Url::parse(&value).context("SEADEXER_PUBLIC_BASE_URL must be a valid URL"))
            .transpose()?;

        let timeout_secs = env::var("SEADEXER_RELEASES_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(10);
        let releases_timeout = Duration::from_secs(timeout_secs);

        let mapping_timeout_secs = env::var("SEADEXER_MAPPING_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(timeout_secs);
        let mapping_timeout = Duration::from_secs(mapping_timeout_secs.max(1));

        let application_title =
            env::var("SEADEXER_TITLE").unwrap_or_else(|_| "Seadexer".to_string());
        let application_description = env::var("SEADEXER_DESCRIPTION")
            .unwrap_or_else(|_| "Indexer bridge for releases.moe".to_string());

        let default_limit = env::var("SEADEXER_DEFAULT_LIMIT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(100);

        let raw_anilist_url = env::var("SEADEXER_ANILIST_BASE_URL")
            .unwrap_or_else(|_| "https://graphql.anilist.co".to_string());
        let anilist_base_url = Url::parse(&raw_anilist_url)
            .context("SEADEXER_ANILIST_BASE_URL must be a valid URL")?;

        let anilist_timeout_secs = env::var("SEADEXER_ANILIST_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(timeout_secs);
        let anilist_timeout = Duration::from_secs(anilist_timeout_secs.max(1));

        let sonarr_enabled = env::var("SEADEXER_SONARR_ENABLED")
            .map(|v| v != "false")
            .unwrap_or(true);

        let sonarr = if sonarr_enabled {
            let raw_sonarr_url = env::var("SONARR_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8989".to_string());
            let sonarr_url = parse_root_url(&raw_sonarr_url, "SONARR_BASE_URL")?;

            let sonarr_api_key =
                env::var("SONARR_API_KEY").context("Missing SONARR_API_KEY variable")?;

            let sonarr_timeout_secs = env::var("SONARR_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(timeout_secs);
            let sonarr_timeout = Duration::from_secs(sonarr_timeout_secs.max(1));

            Some(SonarrConfig {
                url: sonarr_url,
                api_key: sonarr_api_key,
                timeout: sonarr_timeout,
            })
        } else {
            None
        };

        let radarr_enabled = env::var("SEADEXER_RADARR_ENABLED")
            .map(|v| v != "false")
            .unwrap_or(true);

        let radarr = if radarr_enabled {
            let raw_radarr_url = env::var("RADARR_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:7878".to_string());
            let radarr_url = parse_root_url(&raw_radarr_url, "RADARR_BASE_URL")?;

            let radarr_api_key =
                env::var("RADARR_API_KEY").context("Missing RADARR_API_KEY variable")?;

            let radarr_timeout_secs = env::var("RADARR_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(timeout_secs);
            let radarr_timeout = Duration::from_secs(radarr_timeout_secs.max(1));

            Some(RadarrConfig {
                url: radarr_url,
                api_key: radarr_api_key,
                timeout: radarr_timeout,
            })
        } else {
            None
        };

        if sonarr.is_none() && radarr.is_none() {
            anyhow::bail!("At least one of Sonarr or Radarr must be enabled");
        }

        Ok(Self {
            listen_addr,
            public_base_url,
            releases_base_url,
            releases_timeout,
            data_path,
            mapping_source_url,
            mapping_refresh_interval,
            mapping_timeout,
            application_title,
            application_description,
            default_limit,
            anilist_base_url,
            anilist_timeout,
            sonarr,
            radarr,
        })
    }
}

fn parse_root_url(value: &str, label: &str) -> Result<Url> {
    let mut normalized = value.trim().to_string();
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    Url::parse(&normalized).with_context(|| format!("{label} must be a valid URL"))
}
