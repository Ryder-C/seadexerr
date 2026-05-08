use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use reqwest::Url;

pub const RELEASES_BASE_URL: &str = "https://releases.moe/api/";
pub const ANILIST_BASE_URL: &str = "https://graphql.anilist.co";
pub const MAPPING_SOURCE_URL: &str =
    "https://github.com/anibridge/anibridge-mappings/releases/latest/download/mappings.min.json";
pub const MAPPING_REFRESH_INTERVAL: Duration = Duration::from_secs(21_600); // 6 hours
pub const APPLICATION_TITLE: &str = "Seadexerr";
pub const APPLICATION_DESCRIPTION: &str = "Indexer bridge for releases.moe";
pub const DEFAULT_LIMIT: usize = 100;
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const DATA_PATH: &str = "data";

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub listen_addr: SocketAddr,
    pub public_base_url: Option<Url>,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
    pub skip_deband: bool,
    pub prefer_dual_audio: bool,
}

#[derive(Clone, Debug)]
pub struct SonarrConfig {
    pub url: Url,
    pub api_key: String,
}

#[derive(Clone, Debug)]
pub struct RadarrConfig {
    pub url: Url,
    pub api_key: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let host = env::var("SEADEXERR_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = env::var("SEADEXERR_PORT").unwrap_or_else(|_| "6767".to_string());
        let port = port
            .parse::<u16>()
            .context("SEADEXERR_PORT must be a valid u16 integer")?;
        let listen_addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .context("failed to parse socket address from SEADEXERR_HOST and SEADEXERR_PORT")?;

        let public_base_url = env::var("SEADEXERR_PUBLIC_BASE_URL")
            .ok()
            .map(|value| {
                Url::parse(&value).context("SEADEXERR_PUBLIC_BASE_URL must be a valid URL")
            })
            .transpose()?;

        let sonarr = match env::var("SONARR_API_KEY") {
            Ok(api_key) if !api_key.trim().is_empty() => {
                let raw_sonarr_url = env::var("SONARR_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:8989".to_string());
                let sonarr_url = parse_root_url(&raw_sonarr_url, "SONARR_BASE_URL")?;

                Some(SonarrConfig {
                    url: sonarr_url,
                    api_key,
                })
            }
            _ => None,
        };
        let skip_deband = env::var("SEADEXERR_SKIP_DEBAND")
            .map(|v| v != "false")
            .unwrap_or(false);

        let prefer_dual_audio = env::var("SEADEXERR_PREFER_DUAL_AUDIO")
            .map(|v| v != "false")
            .unwrap_or(false);

        let radarr = match env::var("RADARR_API_KEY") {
            Ok(api_key) if !api_key.trim().is_empty() => {
                let raw_radarr_url = env::var("RADARR_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:7878".to_string());
                let radarr_url = parse_root_url(&raw_radarr_url, "RADARR_BASE_URL")?;

                Some(RadarrConfig {
                    url: radarr_url,
                    api_key,
                })
            }
            _ => None,
        };

        if sonarr.is_none() && radarr.is_none() {
            anyhow::bail!(
                "At least one of Sonarr or Radarr must be configured via its API key (SONARR_API_KEY or RADARR_API_KEY)"
            );
        }

        Ok(Self {
            listen_addr,
            public_base_url,
            sonarr,
            radarr,
            skip_deband,
            prefer_dual_audio,
        })
    }
}

pub fn get_data_path() -> PathBuf {
    PathBuf::from(DATA_PATH)
}

fn parse_root_url(value: &str, label: &str) -> Result<Url> {
    let mut normalized = value.trim().to_string();
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    Url::parse(&normalized).with_context(|| format!("{label} must be a valid URL"))
}
