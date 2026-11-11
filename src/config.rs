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
    pub mcp: McpConfig,
    pub sync: SyncSection,
}

/// `[sync.<domain>]`: the domains this machine syncs, each through a git remote. A domain without a
/// section is not synced on this machine.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncSection {
    pub work: Option<SyncConfig>,
    pub personal: Option<SyncConfig>,
    pub shared: Option<SyncConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    /// Any git remote: `git@host:path`, `ssh://...` or `https://...`. A host alias from
    /// `~/.ssh/config` does not work: sync runs ssh with `-F /dev/null` so only the configured key
    /// is offered.
    pub remote: String,
    /// A private key file, or a `.pub` whose private key an agent holds. `~/` is expanded at load.
    pub ssh_key: PathBuf,
    #[serde(default = "default_update_mode")]
    pub update: UpdateMode,
    #[serde(default = "default_pull_every_minutes")]
    pub pull_every_minutes: u64,
}

/// When a domain syncs on its own; a read pulls at most every `pull_every_minutes` in both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMode {
    /// Meant to also sync in the background after each change (not built yet).
    Live,
    /// Sync only when reading.
    OnDemand,
}

/// Often enough that a lesson recorded on one machine shows up soon on another, rarely enough that
/// reads do not hit the network all the time.
const DEFAULT_PULL_EVERY_MINUTES: u64 = 10;

fn default_update_mode() -> UpdateMode {
    UpdateMode::Live
}

fn default_pull_every_minutes() -> u64 {
    DEFAULT_PULL_EVERY_MINUTES
}

impl SyncSection {
    /// The configured domains, in `Domain::ALL` order.
    pub fn configured(&self) -> Vec<(Domain, &SyncConfig)> {
        Domain::ALL
            .iter()
            .filter_map(|&domain| {
                let domain_config: &Option<SyncConfig> = match domain {
                    Domain::Work => &self.work,
                    Domain::Personal => &self.personal,
                    Domain::Shared => &self.shared,
                };
                domain_config
                    .as_ref()
                    .map(|sync_config| (domain, sync_config))
            })
            .collect()
    }
}

/// One local clone: every domain whose remote normalises to the same URL shares it, and so must
/// share its key, because one clone has one `core.sshCommand`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneSpec {
    /// The remote as the first of its domains wrote it; git is given this one.
    pub remote: String,
    pub normalized_remote: String,
    pub ssh_key: PathBuf,
    pub domains: Vec<Domain>,
}

/// Groups the configured domains into clones. Same remote with different keys is an error.
pub fn clone_specs(sync_section: &SyncSection) -> Result<Vec<CloneSpec>> {
    sync_section
        .configured()
        .into_iter()
        .try_fold(Vec::new(), |clones_so_far: Vec<CloneSpec>, (domain, sync_config)| {
            let normalized_remote: String = normalize_remote(&sync_config.remote);
            let (matching_clones, other_clones): (Vec<CloneSpec>, Vec<CloneSpec>) = clones_so_far
                .into_iter()
                .partition(|clone_spec| clone_spec.normalized_remote == normalized_remote);
            let grown_clone: CloneSpec = match matching_clones.into_iter().next() {
                Some(existing_clone) if existing_clone.ssh_key != sync_config.ssh_key => anyhow::bail!(
                    "[sync.{}] and [sync.{}] use the same remote {} with different ssh_key values; \
                     domains sharing a remote must share its key (one clone has one key)",
                    existing_clone.domains.first().map_or("?", |first_domain| first_domain.as_str()),
                    domain.as_str(),
                    crate::sync::redact_credentials(&sync_config.remote)
                ),
                // One clone has one remote text, so the other section's credentials would be dropped silently (a
                // rotated token in only one section would then fail with an auth error pointing nowhere near the
                // config).
                Some(existing_clone)
                    if url_credentials(&existing_clone.remote) != url_credentials(&sync_config.remote) => anyhow::bail!(
                    "[sync.{}] and [sync.{}] name the same repository {} with different credentials; \
                     write the same remote in both",
                    existing_clone.domains.first().map_or("?", |first_domain| first_domain.as_str()),
                    domain.as_str(),
                    crate::sync::redact_credentials(&sync_config.remote)
                ),
                Some(existing_clone) => CloneSpec {
                    domains: existing_clone.domains.into_iter().chain(std::iter::once(domain)).collect(),
                    ..existing_clone
                },
                None => CloneSpec {
                    remote: sync_config.remote.clone(),
                    normalized_remote,
                    ssh_key: sync_config.ssh_key.clone(),
                    domains: vec![domain],
                },
            };
            // The normal form is already credential-free; redacting again is defence in depth for a form that is not.
            journal!(
                "config: [sync.{}] -> clone of {}",
                domain.as_str(),
                crate::sync::redact_credentials(&grown_clone.normalized_remote)
            );
            Ok(other_clones.into_iter().chain(std::iter::once(grown_clone)).collect())
        })
}

/// Warnings to print when syncing: `work` sharing a clone with another domain means work data is
/// readable by everyone with access to that repo, and the work-isolation probe has nothing to test.
pub fn sync_warnings(clones: &[CloneSpec]) -> Vec<String> {
    clones
        .iter()
        .filter(|clone_spec| clone_spec.domains.contains(&Domain::Work) && clone_spec.domains.len() > 1)
        .map(|clone_spec| {
            let other_domains: Vec<&str> = clone_spec
                .domains
                .iter()
                .filter(|domain| **domain != Domain::Work)
                .map(|domain| domain.as_str())
                .collect();
            format!(
                "WARNING: work shares {} with {}: work data is readable by everyone with access to that repo",
                crate::sync::redact_credentials(&clone_spec.remote),
                other_domains.join(" and ")
            )
        })
        .collect()
}

/// The user and password of a non-ssh `scheme://user:password@host/...` URL, which `normalize_remote` drops; an ssh
/// login is a name, not a secret, and is not one.
fn url_credentials(remote: &str) -> Option<&str> {
    remote
        .split_once("://")
        .filter(|(scheme, _)| !scheme.eq_ignore_ascii_case("ssh"))
        .and_then(|(_, after_scheme)| {
            let authority: &str = after_scheme.split('/').next().unwrap_or(after_scheme);
            authority.rsplit_once('@').map(|(userinfo, _)| userinfo)
        })
}

/// The form two remotes are compared in: the host lower-cased, a trailing `/` and `.git` removed.
/// The path keeps its case (it is case-sensitive on some hosts).
fn normalize_remote(remote: &str) -> String {
    let trimmed_remote: &str = remote.trim();
    let fn_lowercase_host = |authority: &str| -> String {
        // `user@host:port` or `user@host`: only the host part is case-insensitive.
        match authority.rsplit_once('@') {
            Some((user_part, host_part)) => format!("{user_part}@{}", host_part.to_lowercase()),
            None => authority.to_lowercase(),
        }
    };
    let host_normalized: String = match trimmed_remote.split_once("://") {
        Some((scheme, after_scheme)) => {
            // A '/', '?' or '#' typed raw inside a password ends the authority early; git wants those percent-encoded,
            // and such a URL is not recognised (as in `redact_credentials`).
            let (authority, path) = after_scheme.split_once('/').unwrap_or((after_scheme, ""));
            let (userinfo, host_part): (Option<&str>, &str) = authority
                .rsplit_once('@')
                .map_or((None, authority), |(userinfo_text, host_text)| {
                    (Some(userinfo_text), host_text)
                });
            let host_lowercase: String = host_part.to_lowercase();
            // The credentials of an http(s) URL are not part of which repository it is, and this form is printed and
            // used in a directory name. An ssh user is a login name and stays; an ssh password (git ignores it)
            // does not.
            let fn_ssh_login_at_host = || -> String {
                userinfo
                    .and_then(|userinfo_text| userinfo_text.split(':').next())
                    .filter(|login| !login.is_empty())
                    .map_or(host_lowercase.clone(), |login| {
                        format!("{login}@{host_lowercase}")
                    })
            };
            match (scheme.eq_ignore_ascii_case("ssh"), host_part.contains(':')) {
                // `ssh://user@host/path` with no port is the same repo as scp-like `user@host:path`, so both get the
                // scp form.
                (true, false) => format!("{}:{path}", fn_ssh_login_at_host()),
                (true, true) => format!("ssh://{}/{path}", fn_ssh_login_at_host()),
                (false, _) => format!("{}://{host_lowercase}/{path}", scheme.to_lowercase()),
            }
        }
        // scp-like `git@host:path`: a ':' before any '/'. Anything else (a local path) is left as is.
        None => match trimmed_remote.split_once(':') {
            Some((authority, path)) if !authority.contains('/') => {
                format!("{}:{path}", fn_lowercase_host(authority))
            }
            Some(_) | None => trimmed_remote.to_string(),
        },
    };
    let without_slash: &str = host_normalized.trim_end_matches('/');
    without_slash
        .strip_suffix(".git")
        .unwrap_or(without_slash)
        .trim_end_matches('/')
        .to_string()
}

/// `~/rest` becomes `<home>/rest`; any other path is returned unchanged.
fn expand_tilde(path: &Path, home_dir: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home_dir) {
        (Ok(rest), Some(home_dir)) => home_dir.join(rest),
        (Ok(_), None) | (Err(_), _) => path.to_path_buf(),
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HookConfig {
    /// Domains the prompt hook may read.
    pub domains: Vec<Domain>,
    pub max_results: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_search: Domain::ALL.to_vec(),
            hook: HookConfig::default(),
            mcp: McpConfig::default(),
            sync: SyncSection::default(),
        }
    }
}

/// Enough lessons to be useful, few enough to stay a short read inside an agent session.
const DEFAULT_AGENT_MAX_RESULTS: usize = 5;

/// The domains an agent (the hook, the MCP server) may touch by default. Personal is left out on
/// purpose: agents run inside work sessions too.
fn default_agent_domains() -> Vec<Domain> {
    vec![Domain::Work, Domain::Shared]
}

impl Default for HookConfig {
    fn default() -> Self {
        HookConfig {
            domains: default_agent_domains(),
            max_results: DEFAULT_AGENT_MAX_RESULTS,
        }
    }
}

/// The MCP server: the only domains its tools may search or record into.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    pub domains: Vec<Domain>,
    /// Search results when a tool call gives no limit.
    pub max_results: usize,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            domains: default_agent_domains(),
            max_results: DEFAULT_AGENT_MAX_RESULTS,
        }
    }
}

pub fn load(lessons_home: &Path) -> Result<Config> {
    let config_path: PathBuf = lessons_home.join("config.toml");
    match std::fs::read_to_string(&config_path) {
        Ok(config_text) => {
            journal!("config: read {}", config_path.display());
            let parsed_config: Config = toml::from_str(&config_text)
                .with_context(|| format!("cannot parse {}", config_path.display()))?;
            let expanded_config: Config =
                with_expanded_keys(parsed_config, dirs::home_dir().as_deref());
            // Checked at load, so a bad [sync] setup fails every command at once, not only a later sync.
            validate_sync_entries(&expanded_config.sync)
                .with_context(|| format!("invalid [sync] in {}", config_path.display()))?;
            clone_specs(&expanded_config.sync)
                .with_context(|| format!("invalid [sync] in {}", config_path.display()))?;
            Ok(expanded_config)
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

/// Rejects values that would misbehave later: a relative `ssh_key` (it depends on the working directory, and two
/// spellings of one key would look like different keys) and a zero `pull_every_minutes` (a pull on every read).
fn validate_sync_entries(sync_section: &SyncSection) -> Result<()> {
    sync_section
        .configured()
        .into_iter()
        .try_for_each(|(domain, sync_config)| {
            match (
                sync_config.ssh_key.is_absolute(),
                sync_config.pull_every_minutes,
            ) {
                (false, _) => anyhow::bail!(
                    "[sync.{}] ssh_key {} must be an absolute path, or start with ~/ where a home directory exists",
                    domain.as_str(),
                    sync_config.ssh_key.display()
                ),
                (true, 0) => anyhow::bail!(
                    "[sync.{}] pull_every_minutes must be at least 1",
                    domain.as_str()
                ),
                (true, _) => Ok(()),
            }
        })
}

/// The config with every `ssh_key` tilde-expanded, so later comparisons and ssh see real paths.
fn with_expanded_keys(config: Config, home_dir: Option<&Path>) -> Config {
    let fn_expand = |domain_config: Option<SyncConfig>| {
        domain_config.map(|sync_config| SyncConfig {
            ssh_key: expand_tilde(&sync_config.ssh_key, home_dir),
            ..sync_config
        })
    };
    Config {
        sync: SyncSection {
            work: fn_expand(config.sync.work),
            personal: fn_expand(config.sync.personal),
            shared: fn_expand(config.sync.shared),
        },
        ..config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed sample, so a change to it that stops it loading fails here.
    const SAMPLE_CONFIG: &str = include_str!("../docs/config.sample.toml");

    fn sync_entry(remote: &str, ssh_key: &str) -> SyncConfig {
        SyncConfig {
            remote: remote.to_string(),
            ssh_key: PathBuf::from(ssh_key),
            update: UpdateMode::Live,
            pull_every_minutes: DEFAULT_PULL_EVERY_MINUTES,
        }
    }

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
        assert!(!config.mcp.domains.contains(&Domain::Personal));
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

    #[test]
    fn the_committed_sample_loads_as_one_clone_with_the_work_warning() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(temp_dir.path().join("config.toml"), SAMPLE_CONFIG).unwrap();
        let config: Config = load(temp_dir.path()).expect("the sample must load");
        let clones: Vec<CloneSpec> = clone_specs(&config.sync).unwrap();
        assert_eq!(
            clones.len(),
            1,
            "the sample names one remote and one key for all three"
        );
        assert_eq!(clones[0].domains, Domain::ALL.to_vec());
        assert_eq!(sync_warnings(&clones).len(), 1);
    }

    #[test]
    fn domains_with_the_same_normalised_remote_share_one_clone() {
        let sync_section = SyncSection {
            work: None,
            personal: Some(sync_entry("git@GitHub.com:owner/repo.git", "/keys/one")),
            shared: Some(sync_entry("git@github.com:owner/repo/", "/keys/one")),
        };
        let clones: Vec<CloneSpec> = clone_specs(&sync_section).unwrap();
        assert_eq!(clones.len(), 1);
        assert_eq!(clones[0].domains, vec![Domain::Personal, Domain::Shared]);
        assert!(
            sync_warnings(&clones).is_empty(),
            "no work domain, so nothing to warn about"
        );
    }

    #[test]
    fn the_same_remote_with_different_keys_is_an_error_naming_both_domains() {
        let different_keys: Result<Config> = load_text(
            "[sync.work]\nremote = \"git@github.com:owner/repo.git\"\nssh_key = \"/keys/work\"\n\n\
             [sync.shared]\nremote = \"git@github.com:owner/repo.git\"\nssh_key = \"/keys/home\"\n",
        );
        let error_text: String = format!(
            "{:#}",
            different_keys.expect_err("different keys on one remote")
        );
        assert!(
            error_text.contains("[sync.work] and [sync.shared]"),
            "{error_text}"
        );
    }

    #[test]
    fn work_sharing_a_remote_gets_the_warning_and_distinct_remotes_do_not() {
        let shared_remote = SyncSection {
            work: Some(sync_entry("git@github.com:owner/repo.git", "/keys/one")),
            personal: None,
            shared: Some(sync_entry("git@github.com:owner/repo.git", "/keys/one")),
        };
        let warnings: Vec<String> = sync_warnings(&clone_specs(&shared_remote).unwrap());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("work shares") && warnings[0].contains("shared"),
            "{warnings:?}"
        );

        let separate_remotes = SyncSection {
            work: Some(sync_entry(
                "git@gitlab.example:company/work-lessons.git",
                "/keys/work",
            )),
            personal: None,
            shared: Some(sync_entry("git@github.com:owner/repo.git", "/keys/home")),
        };
        let separate_clones: Vec<CloneSpec> = clone_specs(&separate_remotes).unwrap();
        assert_eq!(
            separate_clones.len(),
            Domain::ALL.len() - 1,
            "work and shared each get a clone"
        );
        assert!(sync_warnings(&separate_clones).is_empty());
    }

    #[test]
    fn a_credentialed_and_a_plain_spelling_of_one_repository_are_refused_not_merged() {
        let credentialed_and_plain = SyncSection {
            work: Some(sync_entry(
                "https://a:token-one@host.example/r.git",
                "/keys/one",
            )),
            personal: None,
            shared: Some(sync_entry("https://host.example/r.git", "/keys/one")),
        };
        let error_text: String = format!("{:#}", clone_specs(&credentialed_and_plain).unwrap_err());
        assert!(error_text.contains("different credentials"), "{error_text}");
        assert!(!error_text.contains("token"), "{error_text}");
    }

    #[test]
    fn the_work_sharing_warning_never_prints_a_credential_of_the_remote() {
        let credentialed = "https://user:secret-token@github.com/owner/repo.git";
        let shared_remote = SyncSection {
            work: Some(sync_entry(credentialed, "/keys/one")),
            personal: None,
            shared: Some(sync_entry(credentialed, "/keys/one")),
        };
        let warnings: Vec<String> = sync_warnings(&clone_specs(&shared_remote).unwrap());
        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].contains("secret-token"), "{warnings:?}");
    }

    #[test]
    fn sync_sections_refuse_unknown_keys_domains_and_modes_and_need_a_remote() {
        assert!(
            load_text("[sync.work]\nremote = \"r\"\nssh_key = \"/k\"\nremtoe = \"typo\"\n")
                .is_err()
        );
        assert!(load_text("[sync.home]\nremote = \"r\"\nssh_key = \"/k\"\n").is_err());
        assert!(
            load_text("[sync.work]\nremote = \"r\"\nssh_key = \"/k\"\nupdate = \"ondemand\"\n")
                .is_err()
        );
        assert!(load_text("[sync.work]\nssh_key = \"/k\"\n").is_err());
    }

    #[test]
    fn update_defaults_to_live_and_on_demand_parses() {
        let config: Config = load_text(
            "[sync.work]\nremote = \"r\"\nssh_key = \"/k\"\n\n[sync.shared]\nremote = \"s\"\nssh_key = \"/k\"\nupdate = \"on-demand\"\n",
        )
        .unwrap();
        let work_sync: &SyncConfig = config.sync.work.as_ref().unwrap();
        assert_eq!(work_sync.update, UpdateMode::Live);
        assert_eq!(work_sync.pull_every_minutes, DEFAULT_PULL_EVERY_MINUTES);
        assert_eq!(
            config.sync.shared.as_ref().unwrap().update,
            UpdateMode::OnDemand
        );
    }

    #[test]
    fn remotes_normalise_host_case_and_trailing_parts_but_keep_path_case() {
        let normalise_cases: &[(&str, &str)] = &[
            ("git@GitHub.com:Owner/Repo.git", "git@github.com:Owner/Repo"),
            ("git@github.com:Owner/Repo/", "git@github.com:Owner/Repo"),
            (
                "ssh://git@GitHub.com/Owner/Repo.git",
                "git@github.com:Owner/Repo",
            ),
            (
                "ssh://git@GitHub.com:2222/Owner/Repo.git",
                "ssh://git@github.com:2222/Owner/Repo",
            ),
            (
                "HTTPS://GitHub.com/Owner/Repo.git/",
                "https://github.com/Owner/Repo",
            ),
            (
                "  git@github.com:owner/repo.git  ",
                "git@github.com:owner/repo",
            ),
            ("/srv/git/Lessons.git", "/srv/git/Lessons"),
        ];
        normalise_cases
            .iter()
            .for_each(|(written_remote, expected_form)| {
                assert_eq!(
                    normalize_remote(written_remote),
                    *expected_form,
                    "for {written_remote:?}"
                );
            });
    }

    #[test]
    fn an_ssh_password_is_dropped_from_the_normal_form_and_its_login_kept() {
        assert_eq!(
            normalize_remote("ssh://git:secret@GitHub.com:2222/Owner/Repo.git"),
            "ssh://git@github.com:2222/Owner/Repo"
        );
        assert_eq!(
            normalize_remote("ssh://git:secret@GitHub.com/Owner/Repo.git"),
            "git@github.com:Owner/Repo"
        );
    }

    #[test]
    fn one_repository_spelled_with_different_credentials_is_refused_and_the_error_hides_them() {
        let rotated_in_one_section = SyncSection {
            work: Some(sync_entry(
                "https://a:old-token@host.example/r.git",
                "/keys/one",
            )),
            personal: None,
            shared: Some(sync_entry(
                "https://a:new-token@host.example/r.git",
                "/keys/one",
            )),
        };
        let error_text: String = format!("{:#}", clone_specs(&rotated_in_one_section).unwrap_err());
        assert!(error_text.contains("different credentials"), "{error_text}");
        assert!(!error_text.contains("token"), "{error_text}");
    }

    #[test]
    fn spellings_with_the_same_credentials_still_group_whatever_else_differs() {
        let spelling_pairs: &[(&str, &str)] = &[
            (
                "ssh://git@host.example/o/r",
                "ssh://git@host.example/o/r.git",
            ),
            (
                "https://u:tok@host.example/r.git",
                "https://u:tok@HOST.example/r",
            ),
        ];
        spelling_pairs
            .iter()
            .for_each(|(work_remote, shared_remote)| {
                let section = SyncSection {
                    work: Some(sync_entry(work_remote, "/keys/one")),
                    personal: None,
                    shared: Some(sync_entry(shared_remote, "/keys/one")),
                };
                let clones: Vec<CloneSpec> = clone_specs(&section).unwrap();
                assert_eq!(clones.len(), 1, "{work_remote} and {shared_remote}");
            });
    }

    #[test]
    fn an_at_sign_in_an_ssh_password_does_not_leak_into_the_normal_form() {
        assert_eq!(
            normalize_remote("ssh://git:p@ss@Host.example:2222/o/r.git"),
            "ssh://git@host.example:2222/o/r"
        );
    }

    #[test]
    fn an_ssh_url_and_the_scp_form_of_one_repo_share_a_clone() {
        let sync_section = SyncSection {
            work: None,
            personal: Some(sync_entry(
                "ssh://git@github.com/owner/repo.git",
                "/keys/one",
            )),
            shared: Some(sync_entry("git@github.com:owner/repo.git", "/keys/one")),
        };
        assert_eq!(clone_specs(&sync_section).unwrap().len(), 1);
    }

    #[test]
    fn a_relative_ssh_key_and_a_zero_pull_interval_are_refused() {
        let relative_key_error: String = format!(
            "{:#}",
            load_text(
                "[sync.work]\nremote = \"git@github.com:owner/repo.git\"\nssh_key = \"keys/work\"\n"
            )
            .unwrap_err()
        );
        assert!(
            relative_key_error.contains("absolute path"),
            "{relative_key_error}"
        );
        let zero_pull_error: String = format!(
            "{:#}",
            load_text("[sync.work]\nremote = \"git@github.com:owner/repo.git\"\nssh_key = \"/k\"\npull_every_minutes = 0\n").unwrap_err()
        );
        assert!(zero_pull_error.contains("at least 1"), "{zero_pull_error}");
    }

    #[test]
    fn load_expands_a_tilde_in_ssh_key() {
        let config: Config = load_text(
            "[sync.work]\nremote = \"git@github.com:owner/repo.git\"\nssh_key = \"~/.ssh/key\"\n",
        )
        .unwrap();
        let loaded_key: &Path = &config.sync.work.as_ref().unwrap().ssh_key;
        match dirs::home_dir() {
            Some(home_dir) => assert_eq!(loaded_key, home_dir.join(".ssh/key")),
            None => assert_eq!(loaded_key, Path::new("~/.ssh/key")), // no home to expand into
        }
    }

    #[test]
    fn a_leading_tilde_expands_to_home_only() {
        let home_dir = Path::new("/home/someone");
        assert_eq!(
            expand_tilde(Path::new("~/.ssh/key"), Some(home_dir)),
            PathBuf::from("/home/someone/.ssh/key")
        );
        assert_eq!(
            expand_tilde(Path::new("/abs/key"), Some(home_dir)),
            PathBuf::from("/abs/key")
        );
        assert_eq!(
            expand_tilde(Path::new("~/.ssh/key"), None),
            PathBuf::from("~/.ssh/key")
        );
    }
}
