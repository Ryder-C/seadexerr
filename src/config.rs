//! Application configuration loaded from environment variables.
//!
//! At least one of `SONARR_API_KEY` or `RADARR_API_KEY` must be set, everything
//! else has a default.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

use anyhow::{Result, bail};
use reqwest::Url;
use serde::Deserialize;

pub const APPLICATION_TITLE: &str = "Seadexerr";
pub const APPLICATION_DESCRIPTION: &str = "Indexer bridge for releases.moe";
pub const DATA_PATH: &str = "data";

/// How to pick between multiple eligible releases for the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnimePreference {
    /// Use the release marked as Best on releases.moe
    #[default]
    Best,
    /// Prefer releases marked Dual Audio on releases.moe
    DualAudio,
    /// Prefer releases with the smallest file size
    Smallest,
}

#[derive(Deserialize)]
struct EnvConfig {
    #[serde(default = "default_host")]
    seadexerr_host: IpAddr,
    #[serde(default = "default_port")]
    seadexerr_port: u16,
    seadexerr_public_base_url: Option<Url>,
    sonarr_api_key: Option<String>,
    #[serde(default = "default_sonarr_url")]
    sonarr_base_url: Url,
    radarr_api_key: Option<String>,
    #[serde(default = "default_radarr_url")]
    radarr_base_url: Url,

    // Extra configuration
    #[serde(default)]
    seadexerr_skip_deband: bool,
    #[serde(default)]
    seadexerr_prefer: AnimePreference,
    ab_passkey: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub listen_addr: SocketAddr,
    pub public_base_url: Option<Url>,
    pub sonarr: Option<SonarrConfig>,
    pub radarr: Option<RadarrConfig>,
    pub skip_deband: bool,
    pub preference: AnimePreference,
    pub ab_passkey: Option<String>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        envy::from_env::<EnvConfig>()?.try_into()
    }
}

impl TryFrom<EnvConfig> for AppConfig {
    type Error = anyhow::Error;

    fn try_from(env_config: EnvConfig) -> Result<Self, Self::Error> {
        let EnvConfig {
            seadexerr_host,
            seadexerr_port,
            seadexerr_public_base_url,
            sonarr_api_key,
            sonarr_base_url,
            radarr_api_key,
            radarr_base_url,
            seadexerr_skip_deband,
            seadexerr_prefer,
            ab_passkey,
        } = env_config;

        let listen_addr = SocketAddr::new(seadexerr_host, seadexerr_port);

        let public_base_url = seadexerr_public_base_url;

        let sonarr = sonarr_api_key.map(|api_key| SonarrConfig {
            url: sonarr_base_url,
            api_key,
        });

        let radarr = radarr_api_key.map(|api_key| RadarrConfig {
            url: radarr_base_url,
            api_key,
        });

        let skip_deband = seadexerr_skip_deband;

        let preference = seadexerr_prefer;

        if sonarr.is_none() && radarr.is_none() {
            bail!("at least one of Sonarr or Radarr configuration must be provided");
        }

        Ok(AppConfig {
            listen_addr,
            public_base_url,
            sonarr,
            radarr,
            skip_deband,
            preference,
            ab_passkey,
        })
    }
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

fn default_host() -> IpAddr {
    IpAddr::from([0, 0, 0, 0])
}

fn default_port() -> u16 {
    6767
}

fn default_sonarr_url() -> Url {
    Url::parse("http://localhost:8989/").expect("default Sonarr url must be valid")
}

fn default_radarr_url() -> Url {
    Url::parse("http://localhost:7878/").expect("default Radarr url must be valid")
}

pub fn default_data_path() -> PathBuf {
    PathBuf::from(DATA_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_env() -> EnvConfig {
        EnvConfig {
            seadexerr_host: default_host(),
            seadexerr_port: default_port(),
            seadexerr_public_base_url: None,
            sonarr_api_key: None,
            sonarr_base_url: default_sonarr_url(),
            radarr_api_key: None,
            radarr_base_url: default_radarr_url(),
            seadexerr_skip_deband: false,
            seadexerr_prefer: AnimePreference::Best,
            ab_passkey: None,
        }
    }

    #[test]
    fn requires_sonarr_or_radarr() {
        assert!(AppConfig::try_from(base_env()).is_err());
    }

    #[test]
    fn sonarr_only() {
        let env = EnvConfig {
            sonarr_api_key: Some("k".into()),
            ..base_env()
        };

        assert!(AppConfig::try_from(env).is_ok());
    }

    #[test]
    fn radarr_only() {
        let env = EnvConfig {
            radarr_api_key: Some("k".into()),
            ..base_env()
        };

        assert!(AppConfig::try_from(env).is_ok());
    }

    #[test]
    fn sonarr_and_radarr() {
        let env = EnvConfig {
            sonarr_api_key: Some("k".into()),
            radarr_api_key: Some("k".into()),
            ..base_env()
        };

        assert!(AppConfig::try_from(env).is_ok());
    }
}
