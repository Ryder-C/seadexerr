use std::{env, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use reqwest::Url;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub listen_addr: SocketAddr,
    pub public_base_url: Option<Url>,
    pub releases_base_url: Url,
    pub releases_timeout: Duration,
    pub mapping_base_url: Url,
    pub mapping_timeout: Duration,
    pub application_title: String,
    pub application_description: String,
    pub default_limit: usize,
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

        let raw_mapping_base_url = env::var("SEADEXER_MAPPING_BASE_URL")
            .unwrap_or_else(|_| "https://plexanibridge-api.elias.eu.org/".to_string());
        let mapping_base_url = parse_root_url(&raw_mapping_base_url, "SEADEXER_MAPPING_BASE_URL")?;

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
        let mapping_timeout = Duration::from_secs(mapping_timeout_secs);

        let application_title =
            env::var("SEADEXER_TITLE").unwrap_or_else(|_| "Seadexer".to_string());
        let application_description = env::var("SEADEXER_DESCRIPTION")
            .unwrap_or_else(|_| "Indexer bridge for releases.moe".to_string());

        let default_limit = env::var("SEADEXER_DEFAULT_LIMIT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(100);

        Ok(Self {
            listen_addr,
            public_base_url,
            releases_base_url,
            releases_timeout,
            mapping_base_url,
            mapping_timeout,
            application_title,
            application_description,
            default_limit,
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
