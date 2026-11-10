//! `$LESSONS_HOME/config.toml`: optional settings. A missing file means every default applies.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::db::Domain;
use crate::journal;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)] // a misspelled key is an error, not a silently ignored setting
pub struct Config {
    /// Domains `lessons search` uses when `--in` is omitted.
    pub default_search: Vec<Domain>,
    pub hook: HookConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HookConfig {
    /// Domains the Claude Code hook may read.
    pub domains: Vec<Domain>,
    pub max_results: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_search: Domain::ALL.to_vec(),
            hook: HookConfig::default(),
        }
    }
}

impl Default for HookConfig {
    fn default() -> Self {
        HookConfig {
            // Personal is left out on purpose: the hook runs inside work sessions too.
            domains: vec![Domain::Work, Domain::Shared],
            max_results: 5,
        }
    }
}

pub fn load(lessons_home: &Path) -> Result<Config> {
    let config_path: PathBuf = lessons_home.join("config.toml");
    match std::fs::read_to_string(&config_path) {
        Ok(config_text) => {
            journal!("config: read {}", config_path.display());
            toml::from_str(&config_text)
                .with_context(|| format!("cannot parse {}", config_path.display()))
        }
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
            journal!(
                "config: {} not found, using defaults",
                config_path.display()
            );
            Ok(Config::default())
        }
        Err(read_error) => {
            Err(read_error).with_context(|| format!("cannot read {}", config_path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(config_text: &str) -> Result<Config> {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(temp_dir.path().join("config.toml"), config_text).unwrap();
        load(temp_dir.path())
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config: Config = load(temp_dir.path()).unwrap();
        assert_eq!(config, Config::default());
        assert!(!config.hook.domains.contains(&Domain::Personal));
    }

    #[test]
    fn a_partial_file_keeps_the_defaults_it_does_not_name() {
        let config: Config = load_text("default_search = [\"work\"]\n").unwrap();
        assert_eq!(config.default_search, vec![Domain::Work]);
        assert_eq!(config.hook, HookConfig::default());
    }

    #[test]
    fn the_handoff_example_parses() {
        let config: Config = load_text(
            "default_search = [\"work\", \"personal\", \"shared\"]\n\n[hook]\ndomains = [\"work\", \"shared\"]\nmax_results = 5\n",
        )
        .unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn unknown_keys_and_unknown_domains_are_errors() {
        assert!(load_text("default_serach = [\"work\"]\n").is_err());
        assert!(load_text("default_search = [\"home\"]\n").is_err());
    }
}
