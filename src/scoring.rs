use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::releases::Torrent;

pub const SCORING_FILE: &str = "scoring.toml";

/// Default weight for releases marked Best, applied when `best` is unset.
/// Large enough to dominate, so an empty/absent table just prioritizes Best.
fn default_best() -> i32 {
    100
}

/// The scoring table that decides which release(s) get the seeder boost
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoringConfig {
    /// Weight for releases marked Best on releases.moe
    #[serde(default = "default_best")]
    pub best: i32,
    /// Weight for releases marked Dual Audio on releases.moe
    #[serde(default)]
    pub dual_audio: i32,
    /// Applied to the size factor: `<0` favors smaller, `>0` larger, `0` ignores
    #[serde(default)]
    pub size_weight: i32,
    /// Tags that remove a release from results entirely (exact releases.moe label)
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    /// Per-tag weights, keyed by the exact releases.moe tag label
    #[serde(default)]
    pub tags: HashMap<String, i32>,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            best: default_best(),
            dual_audio: 0,
            size_weight: 0,
            exclude_tags: Vec::new(),
            tags: HashMap::new(),
        }
    }
}

impl ScoringConfig {
    /// Load the scoring table from `<data_path>/scoring.toml` with prefer best default
    pub fn load(
        data_path: &Path,
        prefer: Option<LegacyPreference>,
        skip_deband: Option<bool>,
    ) -> Result<Self> {
        if prefer.is_some() {
            warn!("SEADEXERR_PREFER is deprecated -- configure scoring in {SCORING_FILE} instead");
        }
        if skip_deband.is_some() {
            warn!(
                "SEADEXERR_SKIP_DEBAND is deprecated -- use exclude_tags in {SCORING_FILE} instead"
            );
        }

        let path = data_path.join(SCORING_FILE);

        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                info!("loaded scoring config from {}", path.display());
                toml::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::from_legacy(
                prefer.unwrap_or_default(),
                skip_deband.unwrap_or(false),
            )),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Build an equivalent scoring profile from the deprecated env settings.
    fn from_legacy(prefer: LegacyPreference, skip_deband: bool) -> Self {
        let (best, dual_audio, size_weight) = match prefer {
            LegacyPreference::Best => (default_best(), 0, 0),
            LegacyPreference::DualAudio => (0, 1, 0),
            LegacyPreference::Smallest => (0, 0, -1),
        };

        let exclude_tags = if skip_deband {
            vec![DEBAND_REQUIRED_TAG.to_string()]
        } else {
            Vec::new()
        };

        Self {
            best,
            dual_audio,
            size_weight,
            exclude_tags,
            tags: HashMap::new(),
        }
    }

    /// Whether a release should be removed from results based on its tags.
    pub fn is_excluded(&self, tags: &[String]) -> bool {
        self.exclude_tags
            .iter()
            .any(|excluded| tags.iter().any(|tag| tag == excluded))
    }

    /// For each release, whether it is among the highest scorers in the set.
    pub fn priorities(&self, releases: &[Torrent]) -> Vec<bool> {
        if releases.is_empty() {
            return Vec::new();
        }

        let max_size = releases.iter().map(|r| r.size_bytes).max().unwrap_or(0);
        let min_size = releases.iter().map(|r| r.size_bytes).min().unwrap_or(0);
        let span = i128::from(max_size - min_size);

        let scores: Vec<i128> = releases
            .iter()
            .map(|release| {
                let mut base = 0i128;

                if release.is_best {
                    base += i128::from(self.best);
                }
                if release.dual_audio {
                    base += i128::from(self.dual_audio);
                }
                for tag in &release.tags {
                    if let Some(tag_weight) = self.tags.get(tag) {
                        base += i128::from(*tag_weight);
                    }
                }

                if span == 0 {
                    base
                } else {
                    // Scaled by `span`, so this is a large comparison key, not a real score.
                    base * span
                        + i128::from(self.size_weight) * i128::from(release.size_bytes - min_size)
                }
            })
            .collect();

        let best = scores.iter().copied().max().unwrap_or(0);
        scores.iter().map(|&score| score == best).collect()
    }
}

/// The exact releases.moe label for the Deband Required tag.
const DEBAND_REQUIRED_TAG: &str = "Deband Required";

/// The deprecated `SEADEXERR_PREFER` value, kept only to migrate old setups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPreference {
    #[default]
    Best,
    DualAudio,
    Smallest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::releases::{Torrent, Tracker};

    fn torrent(id: &str, size: u64) -> Torrent {
        Torrent {
            id: id.to_string(),
            download_url: String::new(),
            source_url: String::new(),
            info_hash: None,
            published: None,
            files: Vec::new(),
            size_bytes: size,
            is_best: false,
            dual_audio: false,
            tags: Vec::new(),
            anilist_id: None,
            tracker: Tracker::Nyaa,
        }
    }

    #[test]
    fn parses_table() {
        let toml = r#"
            best = 100
            dual_audio = 50
            size_weight = 30
            exclude_tags = ["Deband Required"]

            [tags]
            "HDR" = 20
            "VFR" = -10
        "#;

        let scoring: ScoringConfig = toml::from_str(toml).unwrap();
        assert_eq!(scoring.best, 100);
        assert_eq!(scoring.dual_audio, 50);
        assert_eq!(scoring.size_weight, 30);
        assert_eq!(scoring.exclude_tags, vec!["Deband Required".to_string()]);
        assert_eq!(scoring.tags.get("HDR"), Some(&20));
        assert_eq!(scoring.tags.get("VFR"), Some(&-10));
    }

    #[test]
    fn minimal_table_uses_defaults() {
        let scoring: ScoringConfig = toml::from_str("best = 100\n").unwrap();
        assert_eq!(scoring.best, 100);
        assert_eq!(scoring.dual_audio, 0);
        assert!(scoring.exclude_tags.is_empty());
        assert!(scoring.tags.is_empty());
    }

    #[test]
    fn best_weight_wins() {
        let scoring = ScoringConfig {
            best: 100,
            ..Default::default()
        };
        let mut a = torrent("a", 100);
        a.is_best = true;
        let b = torrent("b", 50);

        assert_eq!(scoring.priorities(&[a, b]), vec![true, false]);
    }

    #[test]
    fn size_weight_favors_smaller() {
        let scoring = ScoringConfig {
            size_weight: -30,
            ..Default::default()
        };
        let big = torrent("big", 1000);
        let small = torrent("small", 100);

        assert_eq!(scoring.priorities(&[big, small]), vec![false, true]);
    }

    #[test]
    fn ties_all_win() {
        let scoring = ScoringConfig {
            best: 100,
            ..Default::default()
        };
        let mut a = torrent("a", 100);
        a.is_best = true;
        let mut b = torrent("b", 100);
        b.is_best = true;

        assert_eq!(scoring.priorities(&[a, b]), vec![true, true]);
    }

    #[test]
    fn empty_set_has_no_priorities() {
        let scoring = ScoringConfig::default();
        assert!(scoring.priorities(&[]).is_empty());
    }

    #[test]
    fn exclude_matches_exact_tag() {
        let scoring = ScoringConfig {
            exclude_tags: vec!["Deband Required".to_string()],
            ..Default::default()
        };
        assert!(scoring.is_excluded(&["Deband Required".to_string()]));
        assert!(!scoring.is_excluded(&["HDR".to_string()]));
        assert!(!scoring.is_excluded(&[]));
    }

    #[test]
    fn load_without_file_or_env_prefers_best() {
        let dir = tempfile::tempdir().unwrap();
        let scoring = ScoringConfig::load(dir.path(), None, None).unwrap();
        assert_eq!(scoring.best, default_best());
        assert_eq!(scoring.dual_audio, 0);
        assert_eq!(scoring.size_weight, 0);
        assert!(scoring.exclude_tags.is_empty());
    }

    #[test]
    fn load_empty_file_matches_no_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SCORING_FILE), "\n  \n").unwrap();
        let scoring = ScoringConfig::load(dir.path(), None, None).unwrap();
        assert_eq!(scoring.best, default_best());
        assert_eq!(scoring.dual_audio, 0);
        assert_eq!(scoring.size_weight, 0);
        assert!(scoring.exclude_tags.is_empty());
    }

    #[test]
    fn load_reads_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SCORING_FILE), "best = 42\n").unwrap();
        let scoring = ScoringConfig::load(dir.path(), None, None).unwrap();
        assert_eq!(scoring.best, 42);
    }

    #[test]
    fn legacy_best_maps_to_best_weight() {
        let scoring = ScoringConfig::from_legacy(LegacyPreference::Best, false);
        assert_eq!(scoring.best, default_best());
        assert_eq!(scoring.dual_audio, 0);
        assert_eq!(scoring.size_weight, 0);
        assert!(scoring.exclude_tags.is_empty());
    }

    #[test]
    fn legacy_smallest_maps_to_size_weight() {
        let scoring = ScoringConfig::from_legacy(LegacyPreference::Smallest, false);
        assert_eq!(scoring.size_weight, -1);
    }

    #[test]
    fn legacy_skip_deband_maps_to_exclude() {
        let scoring = ScoringConfig::from_legacy(LegacyPreference::DualAudio, true);
        assert_eq!(scoring.dual_audio, 1);
        assert_eq!(scoring.exclude_tags, vec!["Deband Required".to_string()]);
    }
}
