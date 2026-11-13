//! The access guards (plan steps S5a and S5b): what a probe's stderr means, whether `work` may sync given what every
//! configured key can read (R6), and what the visibility probe records and warns about (R13). The functions are pure
//! (no git, no ssh, no clock) except `probe_remote`, which runs git and ssh and is exercised by hand.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::files::{ClonePaths, SyncState, read_state};
use super::git::{GitRunner, identity_agent_of, redact_credentials, ssh_command};
use crate::config::CloneSpec;
use crate::journal;

/// What one `git ls-remote` probe says about a key (or, anonymously, about the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeClass {
    /// Exit 0: the caller can read the repository.
    Readable,
    /// The host judged the caller and refused: it said no access, or it rejected the offered key.
    AuthDenied,
    /// No verdict: a transport error, a key that could not be offered or signed, or a message nobody recognises.
    /// Never read as `AuthDenied`.
    Inconclusive,
}

/// What the last visibility probe found, recorded in the clone's state file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
    /// Not probed yet, or the probe was inconclusive: prints nothing and is probed again at the next recheck.
    #[default]
    Unknown,
}

/// The verdict of the R6 guard for the `work` domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    /// This cycle is refused; the reason is logged and shown. A refusal is never cached as a pass.
    Refuse(String),
}

/// Lower-case stderr fragments of an anonymous HTTPS probe that was turned away. Only a probe with no `ssh -v` lines
/// may read these: as plain words in ssh output (a proxy, a local filter) they say nothing about the host's verdict.
/// Fragments for other hosts join this list only once S5 has seen them by hand.
const ANONYMOUS_HTTPS_REFUSALS: &[&str] = &[
    "repository not found",
    "access denied",
    "terminal prompts disabled",
    "could not read username",
    "authentication failed",
];

/// Lower-case fragments an ssh probe may read as the host refusing a key it ACCEPTED: GitHub prints "Repository not
/// found" only after authentication.
const SSH_HOST_REFUSALS: &[&str] = &["repository not found"];

/// Lower-case fragments of the server closing the door on a key it was offered and did not accept. The offer alone is
/// not enough: a connection that dropped after the offer prints the offer too.
const SSH_KEY_REJECTIONS: &[&str] = &["permission denied (publickey"];

/// Lower-case stderr fragments of an ssh agent that holds the key but did not sign: the host never judged the key.
const SIGNING_FAILURES: &[&str] = &["signing failed", "agent refused operation"];

/// What a probe's exit status and stderr mean. The stderr of an ssh probe is `ssh -v` output, so the lines say how far
/// authentication got with THIS key: offered, not accepted and then rejected means the server said no (`AuthDenied`);
/// accepted and then a signing failure means the host never judged it (`Inconclusive`); no offer at all (a missing or
/// passphrase-protected key, a host-key failure) or a connection that dropped is `Inconclusive` too. An unrecognised
/// message is never `AuthDenied`.
pub fn classify_probe(exit_ok: bool, stderr: &str) -> ProbeClass {
    // Order matters: a signing failure outranks a refusal, and anything unrecognised falls to Inconclusive, so a new
    // host's wording can never turn an unknown into a pass of the R6 guard. The wording that counts as a refusal
    // depends on the probe: ssh output (it has `debug1:` lines) needs the key offered and rejected, or accepted and
    // then refused by the host; an anonymous HTTPS probe has no key and reads the plain refusal messages.
    let lowered_stderr: String = stderr.to_lowercase();
    let fn_says_any = |fragments: &[&str]| {
        fragments
            .iter()
            .any(|fragment| lowered_stderr.contains(fragment))
    };
    let key_accepted: bool = lowered_stderr.contains("server accepts key");
    let key_offered_and_rejected: bool = lowered_stderr.contains("offering public key")
        && !key_accepted
        && fn_says_any(SSH_KEY_REJECTIONS);
    let host_refused: bool = match lowered_stderr.contains("debug1:") {
        true => key_offered_and_rejected || (key_accepted && fn_says_any(SSH_HOST_REFUSALS)),
        false => fn_says_any(ANONYMOUS_HTTPS_REFUSALS),
    };
    match (exit_ok, fn_says_any(SIGNING_FAILURES), host_refused) {
        (true, _, _) => ProbeClass::Readable,
        (false, true, _) => ProbeClass::Inconclusive,
        (false, false, true) => ProbeClass::AuthDenied,
        (false, false, false) => ProbeClass::Inconclusive,
    }
}

/// What the visibility probe records. `url_derivable` is false when no `https://<host>/<path>` can be derived from the
/// remote (an ssh alias, a custom port): nothing was probed, so nothing is known.
pub fn visibility_from_probe(class: ProbeClass, url_derivable: bool) -> Visibility {
    // A repository an anonymous caller can read is public; one it is turned away from is private; anything else is
    // unknown rather than a guess, because a wrong "private" would hide a public repository.
    match (url_derivable, class) {
        (false, _) => Visibility::Unknown,
        (true, ProbeClass::Readable) => Visibility::Public,
        (true, ProbeClass::AuthDenied) => Visibility::Private,
        (true, ProbeClass::Inconclusive) => Visibility::Unknown,
    }
}

/// The caution printed on stderr by every command that touches a domain whose remote is recorded `public`, or none.
/// `remote` may carry credentials; they never reach the line.
pub fn public_warning(remote: &str, visibility: Visibility) -> Option<String> {
    // Only a recorded `public` warns: `private` and `unknown` print nothing (R13, a caution and never a constraint).
    match visibility {
        Visibility::Public => Some(format!(
            concat!(
                "WARNING: {} is PUBLIC: anyone can read these lessons and their history; ",
                "change it in the repo's Settings > Danger Zone"
            ),
            redact_credentials(remote)
        )),
        Visibility::Private | Visibility::Unknown => None,
    }
}

/// The R6 guard for a `work` domain that has a remote of its own. `work_probe` is what the WORK key can do against that
/// remote; `other_keys` is every other configured key, by name, with what it can do. Fail closed: the work key must be
/// able to read (else the guard cannot judge), and no other key may read or be unjudged.
pub fn decide_access(work_probe: ProbeClass, other_keys: &[(&str, ProbeClass)]) -> AccessDecision {
    // A key that CAN read is named first, because that is the breach; an unjudged key is named only when none can read.
    let fn_first_key_that = |wanted: ProbeClass| {
        other_keys
            .iter()
            .find(|(_, probe_class)| *probe_class == wanted)
            .map(|(key_name, _)| *key_name)
    };
    match (
        work_probe,
        fn_first_key_that(ProbeClass::Readable),
        fn_first_key_that(ProbeClass::Inconclusive),
    ) {
        (ProbeClass::Readable, None, None) => AccessDecision::Allow,
        (ProbeClass::Readable, Some(key_name), _) => {
            AccessDecision::Refuse(format!("key {key_name} can read the work remote"))
        }
        (ProbeClass::Readable, None, Some(key_name)) => AccessDecision::Refuse(format!(
            "could not tell whether key {key_name} can read the work remote"
        )),
        (ProbeClass::AuthDenied | ProbeClass::Inconclusive, _, _) => {
            AccessDecision::Refuse(format!(
                "the work key could not read the work remote ({work_probe:?}), so the guard cannot judge"
            ))
        }
    }
}

/// How often a remote's visibility is probed again, in days: a repository made public later is caught at the next
/// recheck, not instantly.
pub const ACCESS_RECHECK_DAYS: u64 = 7;

/// Seconds in a day, for the recheck interval.
const SECS_PER_DAY: u64 = 86_400;

/// One probe to run: a `git ls-remote` that answers with a `ProbeClass`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeRequest {
    /// `remote` read with `key` only (ssh `-v`, so the output says how far authentication got): the R6 probes.
    WithKey { remote: String, key: PathBuf },
    /// The anonymous HTTPS `url` read with no identity and no prompt: the R13 visibility probe.
    Anonymous { url: String },
}

/// Runs a probe. The cycle takes it as a parameter so tests feed fake answers and never touch the network.
pub type ProbeFn<'probe> = &'probe dyn Fn(&ProbeRequest) -> ProbeClass;

/// The R6 inputs of a clone that carries `work` with a remote of its own: the work key, and every other configured
/// key by the name of its domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGuard {
    pub work_key: PathBuf,
    pub other_keys: Vec<(String, PathBuf)>,
}

/// The anonymous `https://<host>/<path>` of a remote, or none when it cannot be derived (an ssh alias, a custom port).
pub fn https_url_of_remote(remote: &str) -> Option<String> {
    // Credentials are stripped, and a custom port or a host with no dot is refused: the anonymous probe must not carry a
    // secret, and an ssh alias or a port names no public host.
    let fn_host_and_path = |authority: &str, path: &str| -> Option<String> {
        let host: &str = authority
            .rsplit_once('@')
            .map_or(authority, |(_credentials, host)| host);
        // A '/' in the host means a local path with a colon in it, not a host. A port, a host with no dot and an empty path
        // are refused too: none of them names a public host.
        (host.contains('.') && !host.contains(':') && !host.contains('/') && !path.is_empty())
            .then(|| format!("https://{host}/{}", path.trim_start_matches('/')))
    };
    match remote.split_once("://") {
        Some(("https" | "ssh", rest)) => rest
            .split_once('/')
            .and_then(|(authority, path)| fn_host_and_path(authority, path)),
        Some(_) => None,
        None => remote
            .split_once(':')
            .and_then(|(authority, path)| fn_host_and_path(authority, path)),
    }
}

/// Whether the visibility is due for a probe: never checked, or the last check is `ACCESS_RECHECK_DAYS` old or more.
pub fn access_check_due(last_check_unix_secs: Option<u64>, now_secs: u64) -> bool {
    // An interval old or older is due; a check time in the future (a clock that moved back) is not, so a wrong clock cannot
    // make every cycle probe.
    last_check_unix_secs.is_none_or(|last_check| {
        now_secs.saturating_sub(last_check) >= ACCESS_RECHECK_DAYS * SECS_PER_DAY
    })
}

/// Seconds a probe may take to connect (ssh `ConnectTimeout`) or to move any data (git's low-speed time): a black-holed
/// host must fail the probe, not hold the clone lock.
pub const PROBE_TIMEOUT_SECS: u64 = 10;

/// The wall-clock limit of one probe, in seconds: git has no connect timeout of its own, so a black-holed host would hold
/// the clone lock for minutes without it. The probe is killed and reads as Inconclusive.
pub const PROBE_DEADLINE_SECS: u64 = 30;

/// Bytes per second below which an HTTPS probe counts as stalled for `PROBE_TIMEOUT_SECS`.
pub const PROBE_LOW_SPEED_BYTES_PER_SEC: u64 = 1000;

/// What one probe runs: the ssh command git is told to use, the extra environment, and the arguments of git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeCommand {
    pub ssh_command: String,
    pub env: Vec<(String, String)>,
    /// Variables git must not inherit from the calling process.
    pub env_remove: Vec<String>,
    pub git_args: Vec<String>,
}

/// The ssh command of a key probe: the key alone, verbose so `classify_probe` can read how far authentication got, and a
/// connect timeout so a black-holed host fails the probe.
fn ssh_probe_command(key: &Path, identity_agent: Option<&str>) -> String {
    // Only the key given is offered (`-F /dev/null` in ssh_command drops the user's ssh config); `-v` lets classify_probe
    // read how far authentication got; ConnectTimeout makes a black-holed host fail the probe.
    format!(
        "{} -v -o ConnectTimeout={PROBE_TIMEOUT_SECS}",
        ssh_command(key, identity_agent)
    )
}

/// The environment of an anonymous probe: no askpass, no global, system or XDG git config, no home directory (so no
/// netrc), and git's low-speed limits so a stalled transfer ends.
fn anonymous_probe_env() -> Vec<(String, String)> {
    // TODO(human): begin block 7/8
    // No stored login can make a private repository look public: the shared git-config isolation first, then no askpass, no
    // XDG config and no HOME (a netrc). Git's low-speed limits end a stalled transfer.
    // was: [
    // was:     ("GIT_ASKPASS", ""),
    // was:     ("GIT_CONFIG_GLOBAL", "/dev/null"),
    // was:     ("GIT_CONFIG_NOSYSTEM", "1"),
    // was:     // Older git ignores GIT_CONFIG_GLOBAL and reads the XDG file instead.
    // was:     ("XDG_CONFIG_HOME", "/dev/null"),
    // was:     ("HOME", "/dev/null"),
    // was:     (
    // was:         "GIT_HTTP_LOW_SPEED_LIMIT",
    // was:         &PROBE_LOW_SPEED_BYTES_PER_SEC.to_string(),
    // was:     ),
    // was:     ("GIT_HTTP_LOW_SPEED_TIME", &PROBE_TIMEOUT_SECS.to_string()),
    // was: ]
    // was: .iter()
    // was: .map(|(name, value)| (name.to_string(), value.to_string()))
    // was: .collect()
    // let low_speed_limit: String = PROBE_LOW_SPEED_BYTES_PER_SEC.to_string();
    // let low_speed_time: String = PROBE_TIMEOUT_SECS.to_string();
    // let own_env: Vec<(&str, &str)> = vec![
    //     ("GIT_ASKPASS", ""),
    //     // Older git ignores GIT_CONFIG_GLOBAL and reads the XDG file instead.
    //     ("XDG_CONFIG_HOME", "/dev/null"),
    //     ("HOME", "/dev/null"),
    //     ("GIT_HTTP_LOW_SPEED_LIMIT", &low_speed_limit),
    //     ("GIT_HTTP_LOW_SPEED_TIME", &low_speed_time),
    // ];
    // isolated_git_config_env()
    //     .into_iter()
    //     .chain(
    //         own_env
    //             .iter()
    //             .map(|(name, value)| (name.to_string(), value.to_string())),
    //     )
    //     .collect()
    // TODO(human): end block 7/8
}

/// Git must read no global and no system config: an `insteadOf` there could send a probe to another URL, read with other
/// credentials. Shared by both probes.
fn isolated_git_config_env() -> Vec<(String, String)> {
    [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .to_vec()
}

/// Variables that inject git config (an `insteadOf`, a proxy) from the calling process. Both probes drop them.
fn injected_config_vars() -> Vec<String> {
    ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"]
        .map(String::from)
        .to_vec()
}

/// Proxy variables: a proxy's "access denied" page must not read as a private repository, so the anonymous probe drops
/// them (a host reachable only through a proxy then reads as unknown, the safe direction).
fn proxy_vars() -> Vec<String> {
    [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ]
    .map(String::from)
    .to_vec()
}

/// The key probe: `remote` read with `key` alone, through `ssh_probe_command`.
fn key_probe_command(remote: &str, key: &Path, identity_agent: Option<&str>) -> ProbeCommand {
    // TODO(human): begin block 6/8
    // The key probe must really use the key: git config is isolated and nothing injected through the environment reaches it.
    // HOME stays (ssh needs it for known_hosts) and so does the agent socket.
    // was: ProbeCommand {
    // was:     ssh_command: ssh_probe_command(key, identity_agent),
    // was:     env: Vec::new(),
    // was:     env_remove: Vec::new(),
    // was:     git_args: ["ls-remote", "--", remote, "HEAD"]
    // was:         .map(String::from)
    // was:         .to_vec(),
    // was: }
    // ProbeCommand {
    //     ssh_command: ssh_probe_command(key, identity_agent),
    //     env: isolated_git_config_env(),
    //     env_remove: injected_config_vars(),
    //     git_args: ["ls-remote", "--", remote, "HEAD"]
    //         .map(String::from)
    //         .to_vec(),
    // }
    // TODO(human): end block 6/8
}

/// The anonymous probe: `url` read with no identity, no credential helper, no redirect and no config injected through
/// the environment (an `insteadOf` or a proxy there must not reach it).
fn anonymous_probe_command(url: &str) -> ProbeCommand {
    // TODO(human): begin block 8/8
    // The anonymous probe is isolated three ways: its environment (anonymous_probe_env), the variables it must not inherit
    // (injected git config and every proxy: a proxy's access-denied page must not read as a private repository) and git's own
    // switches (no credential helper, no redirect: a renamed repository stays unknown, the safe direction).
    // was: ProbeCommand {
    // was:     ssh_command: "ssh".to_string(), // never run: the URL is https
    // was:     env: anonymous_probe_env(),
    // was:     env_remove: ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"]
    // was:         .map(String::from)
    // was:         .to_vec(),
    // was:     git_args: [
    // was:         "-c",
    // was:         "credential.helper=",
    // was:         "-c",
    // was:         "http.followRedirects=false",
    // was:         "ls-remote",
    // was:         "--",
    // was:         url,
    // was:         "HEAD",
    // was:     ]
    // was:     .map(String::from)
    // was:     .to_vec(),
    // was: }
    // ProbeCommand {
    //     ssh_command: "ssh".to_string(), // never run: the URL is https
    //     env: anonymous_probe_env(),
    //     env_remove: [injected_config_vars(), proxy_vars()].concat(),
    //     git_args: [
    //         "-c",
    //         "credential.helper=",
    //         "-c",
    //         "http.followRedirects=false",
    //         "ls-remote",
    //         "--",
    //         url,
    //         "HEAD",
    //     ]
    //     .map(String::from)
    //     .to_vec(),
    // }
    // TODO(human): end block 8/8
}

/// The command of one probe, built without running anything so its isolation and its timeouts can be tested.
/// `identity_agent` is the agent socket the user's ssh configuration names for the host, if any.
pub fn probe_command_of(request: &ProbeRequest, identity_agent: Option<&str>) -> ProbeCommand {
    match request {
        ProbeRequest::WithKey { remote, key } => key_probe_command(remote, key, identity_agent),
        ProbeRequest::Anonymous { url } => anonymous_probe_command(url),
    }
}

/// A git runner for `command`: its ssh command and environment set, the variables it must not inherit removed.
fn runner_for(command: &ProbeCommand) -> GitRunner {
    // The runner carries the command's ssh command and environment, then drops the variables the probe must not inherit.
    let runner_with_env: GitRunner = command.env.iter().fold(
        GitRunner::new(&std::env::temp_dir(), &command.ssh_command),
        |runner_so_far, (name, value)| runner_so_far.with_env(name, value),
    );
    command
        .env_remove
        .iter()
        .fold(runner_with_env, |runner_so_far, name| {
            runner_so_far.without_env(name)
        })
}

/// The real probe: `git ls-remote` as the request says, its exit status and stderr read by `classify_probe`. Exercised
/// by hand against every configured host (S5), never by the tests.
pub fn probe_remote(request: &ProbeRequest) -> ProbeClass {
    // Every outcome becomes a class: a git that cannot start, or that outlives PROBE_DEADLINE_SECS, is Inconclusive and
    // never a pass. Running the command is exercised by hand against every configured host.
    let agent: Option<String> = match request {
        ProbeRequest::WithKey { remote, .. } => identity_agent_of(remote),
        ProbeRequest::Anonymous { .. } => None,
    };
    let command: ProbeCommand = probe_command_of(request, agent.as_deref());
    let git_args: Vec<&str> = command.git_args.iter().map(String::as_str).collect();
    match runner_for(&command)
        .run_with_deadline(&git_args, Duration::from_secs(PROBE_DEADLINE_SECS))
    {
        Ok(output) => classify_probe(output.exit_ok, &output.stderr),
        Err(spawn_or_deadline_error) => {
            journal!("probe: git gave no answer: {spawn_or_deadline_error:#}");
            ProbeClass::Inconclusive
        }
    }
}

/// The R6 decision for a `work` clone with its own remote: probe the work key, then every other key, and let
/// `decide_access` judge.
pub fn work_guard_decision(work_remote: &str, guard: &WorkGuard, probe: ProbeFn) -> AccessDecision {
    // Probe the work key first, then every other key, and let decide_access judge: the guard fails closed, so nothing here
    // may turn an unjudged key into a pass.
    let fn_probe_with = |key: &Path| {
        probe(&ProbeRequest::WithKey {
            remote: work_remote.to_string(),
            key: key.to_path_buf(),
        })
    };
    let work_probe: ProbeClass = fn_probe_with(&guard.work_key);
    let other_probes: Vec<(&str, ProbeClass)> = guard
        .other_keys
        .iter()
        .map(|(key_name, key)| (key_name.as_str(), fn_probe_with(key)))
        .collect();
    decide_access(work_probe, &other_probes)
}

/// The state after the visibility step: when a probe is due and a URL can be derived, probe anonymously and record the
/// verdict and the check time; an inconclusive probe changes nothing, so the old verdict stays and the next cycle probes again.
pub fn visibility_after_probe(
    state: SyncState,
    remote: &str,
    probe: ProbeFn,
    now_secs: u64,
) -> SyncState {
    // An inconclusive probe keeps the old verdict and the old check time: a flaky network must not drop a known-public
    // warning, and the next cycle probes again because only a verdict stamps the time.
    let due_url: Option<String> = https_url_of_remote(remote)
        .filter(|_| access_check_due(state.last_access_check_unix_secs, now_secs));
    match due_url {
        None => state,
        Some(url) => {
            let class: ProbeClass = probe(&ProbeRequest::Anonymous { url });
            SyncState {
                visibility: match class {
                    ProbeClass::Inconclusive => state.visibility,
                    ProbeClass::Readable | ProbeClass::AuthDenied => {
                        visibility_from_probe(class, true)
                    }
                },
                last_access_check_unix_secs: (class != ProbeClass::Inconclusive)
                    .then_some(now_secs)
                    .or(state.last_access_check_unix_secs),
                ..state
            }
        }
    }
}

/// The PUBLIC warnings to print on stderr for the configured clones: one per clone whose recorded visibility is public,
/// read from the clones' state files (no network).
pub fn public_warnings(lessons_home: &Path, clones: &[CloneSpec]) -> Vec<String> {
    // State files only, no network: this runs before every command.
    clones
        .iter()
        .filter_map(|clone| {
            let state: SyncState =
                read_state(&ClonePaths::new(lessons_home, &clone.normalized_remote).state_file());
            public_warning(&clone.remote, state.visibility)
        })
        .collect()
}

// Table tests over the pure decisions. Each row is a case from the plan's Guards and Public warning rows, and each
// guard was shown red by removing its branch (recorded in the step result).
#[cfg(test)]
mod tests {
    use super::*;

    /// The `ssh -v` lines of a key the server never accepted, ending in the host's rejection.
    const SSH_KEY_OFFERED_NOT_ACCEPTED: &str = "debug1: Offering public key: /k/id_work ED25519 SHA256:abc\n\
         debug1: Authentications that can continue: publickey\n\
         git@github.com: Permission denied (publickey).\n\
         fatal: Could not read from remote repository.\n";
    const SSH_KEY_ACCEPTED_THEN_NO_REPO: &str = "debug1: Offering public key: /k/id_other ED25519 SHA256:abc\n\
         debug1: Server accepts key: /k/id_other ED25519 SHA256:abc\n\
         ERROR: Repository not found.\n\
         fatal: Could not read from remote repository.\n";
    const SSH_KEY_ACCEPTED_THEN_AGENT_REFUSED: &str = "debug1: Offering public key: /k/id_work ED25519 SHA256:abc\n\
         debug1: Server accepts key: /k/id_work ED25519 SHA256:abc\n\
         sign_and_send_pubkey: signing failed for ED25519 \"/k/id_work\" from agent: agent refused operation\n\
         git@github.com: Permission denied (publickey).\n";
    const SSH_KEY_FILE_MISSING: &str = "debug1: identity file /k/nope type -1\n\
         debug1: Will attempt key: /k/nope\n\
         git@github.com: Permission denied (publickey).\n";
    const HTTPS_ANONYMOUS_PRIVATE: &str =
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n";

    /// An offered key and then a connection that dropped is no verdict: the host never said no.
    const SSH_KEY_OFFERED_THEN_CONNECTION_RESET: &str = "debug1: Offering public key: /k/id_other ED25519 SHA256:abc\n\
         Connection reset by 192.0.2.1 port 22\n\
         fatal: Could not read from remote repository.\n";
    const SSH_KEY_ACCEPTED_THEN_UNRECOGNISED: &str = "debug1: Offering public key: /k/id_other ED25519 SHA256:abc\n\
         debug1: Server accepts key: /k/id_other ED25519 SHA256:abc\n\
         something no table has ever seen\n";

    /// The server took the key and the session then failed for another reason: the key was not the problem.
    const SSH_KEY_ACCEPTED_THEN_SESSION_FAILED: &str = "debug1: Offering public key: /k/id_other ED25519 SHA256:abc\n\
         debug1: Server accepts key: /k/id_other ED25519 SHA256:abc\n\
         git@github.com: Permission denied (publickey).\n";

    #[test]
    fn classify_probe_maps_each_stderr_family_to_its_class() {
        let cases: &[(bool, &str, ProbeClass)] = &[
            (true, "", ProbeClass::Readable),
            (
                true,
                SSH_KEY_OFFERED_NOT_ACCEPTED,
                ProbeClass::Readable, // an exit of 0 wins over whatever stderr says
            ),
            (false, SSH_KEY_OFFERED_NOT_ACCEPTED, ProbeClass::AuthDenied),
            (false, SSH_KEY_ACCEPTED_THEN_NO_REPO, ProbeClass::AuthDenied),
            (
                false,
                SSH_KEY_ACCEPTED_THEN_AGENT_REFUSED,
                ProbeClass::Inconclusive,
            ),
            (false, SSH_KEY_FILE_MISSING, ProbeClass::Inconclusive),
            (false, HTTPS_ANONYMOUS_PRIVATE, ProbeClass::AuthDenied),
            (
                false,
                "fatal: unable to access 'https://h/r/': Could not resolve host: h\n",
                ProbeClass::Inconclusive,
            ),
            (
                false,
                "ssh: connect to host h port 22: Connection timed out\n",
                ProbeClass::Inconclusive,
            ),
            (
                false,
                "Host key verification failed.\n",
                ProbeClass::Inconclusive,
            ),
            (false, "", ProbeClass::Inconclusive),
            (
                false,
                "something no table has ever seen\n",
                ProbeClass::Inconclusive,
            ),
        ];
        cases.iter().for_each(|(exit_ok, stderr, expected_class)| {
            assert_eq!(
                classify_probe(*exit_ok, stderr),
                *expected_class,
                "exit_ok {exit_ok}, stderr {stderr:?}"
            );
        });
    }

    #[test]
    fn classify_probe_ignores_the_case_of_the_hosts_wording() {
        assert_eq!(
            classify_probe(false, "ERROR: REPOSITORY NOT FOUND.\n"),
            ProbeClass::AuthDenied
        );
    }

    #[test]
    fn a_signing_failure_outranks_a_no_access_message() {
        let signing_failure_then_no_repo_stderr: String =
            format!("{SSH_KEY_ACCEPTED_THEN_AGENT_REFUSED}ERROR: Repository not found.\n");
        assert_eq!(
            classify_probe(false, &signing_failure_then_no_repo_stderr),
            ProbeClass::Inconclusive
        );
    }

    #[test]
    fn decide_access_refuses_unless_the_work_key_reads_and_no_other_key_can() {
        use AccessDecision::{Allow, Refuse};
        use ProbeClass::{AuthDenied, Inconclusive, Readable};
        let fn_refusal_names = |decision: &AccessDecision, key_name: &str| match decision {
            Refuse(reason) => reason.contains(key_name),
            Allow => false,
        };
        assert_eq!(decide_access(Readable, &[]), Allow, "no other key at all");
        assert_eq!(
            decide_access(Readable, &[("home", AuthDenied), ("spare", AuthDenied)]),
            Allow
        );
        assert!(fn_refusal_names(
            &decide_access(Readable, &[("home", Readable)]),
            "home"
        ));
        assert!(fn_refusal_names(
            &decide_access(Readable, &[("home", AuthDenied), ("spare", Inconclusive)]),
            "spare"
        ));
        assert!(
            fn_refusal_names(
                &decide_access(Readable, &[("spare", Inconclusive), ("home", Readable)]),
                "home"
            ),
            "a key that can read is named before one that is unjudged"
        );
        assert!(matches!(
            decide_access(AuthDenied, &[]),
            Refuse(reason) if reason.contains("work key") && reason.contains("AuthDenied")
        ));
        assert!(matches!(
            decide_access(Inconclusive, &[("home", AuthDenied)]),
            Refuse(reason) if reason.contains("work key") && reason.contains("Inconclusive")
        ));
    }

    #[test]
    fn visibility_is_unknown_unless_the_probe_judged_a_derivable_url() {
        let cases: &[(ProbeClass, bool, Visibility)] = &[
            (ProbeClass::Readable, true, Visibility::Public),
            (ProbeClass::AuthDenied, true, Visibility::Private),
            (ProbeClass::Inconclusive, true, Visibility::Unknown),
            (ProbeClass::Readable, false, Visibility::Unknown),
            (ProbeClass::AuthDenied, false, Visibility::Unknown),
            (ProbeClass::Inconclusive, false, Visibility::Unknown),
        ];
        cases
            .iter()
            .for_each(|(class, url_derivable, expected_visibility)| {
                assert_eq!(
                    visibility_from_probe(*class, *url_derivable),
                    *expected_visibility,
                    "{class:?}, url_derivable {url_derivable}"
                );
            });
    }

    #[test]
    fn only_a_public_remote_warns_and_the_line_hides_credentials() {
        let remote: &str = "https://user:secret-token@github.com/owner/repo";
        let warning: String = public_warning(remote, Visibility::Public).unwrap();
        assert!(warning.starts_with("WARNING: "), "{warning}");
        assert!(warning.contains("github.com/owner/repo"), "{warning}");
        assert!(warning.contains("PUBLIC"), "{warning}");
        assert!(!warning.contains("secret-token"), "{warning}");
        assert_eq!(public_warning(remote, Visibility::Private), None);
        assert_eq!(public_warning(remote, Visibility::Unknown), None);
    }

    #[test]
    fn an_unprobed_remote_is_unknown_by_default() {
        assert_eq!(Visibility::default(), Visibility::Unknown);
    }

    #[test]
    fn an_offer_without_a_rejection_is_no_verdict() {
        assert_eq!(
            classify_probe(false, SSH_KEY_OFFERED_THEN_CONNECTION_RESET),
            ProbeClass::Inconclusive,
            "a dropped connection after the offer must not read as the key being rejected"
        );
    }

    #[test]
    fn an_accepted_key_followed_by_a_failed_session_is_no_verdict() {
        assert_eq!(
            classify_probe(false, SSH_KEY_ACCEPTED_THEN_SESSION_FAILED),
            ProbeClass::Inconclusive
        );
    }

    #[test]
    fn an_accepted_key_with_an_unrecognised_message_is_no_verdict() {
        assert_eq!(
            classify_probe(false, SSH_KEY_ACCEPTED_THEN_UNRECOGNISED),
            ProbeClass::Inconclusive,
            "'Server accepts key' must stop the offered-not-accepted rule from applying"
        );
    }

    #[test]
    fn each_anonymous_https_refusal_is_denied_on_its_own() {
        let single_fragment_cases: &[&str] = &[
            "remote: Repository not found.\nfatal: repository 'https://h/r/' not found\n",
            "remote: Access denied\n",
            "fatal: terminal prompts disabled\n",
            "fatal: could not read Username for 'https://h': terminal prompts disabled\n",
            "fatal: could not read Username for 'https://h': No such device or address\n",
            "fatal: Authentication failed for 'https://h/r/'\n",
        ];
        single_fragment_cases.iter().for_each(|stderr| {
            assert_eq!(
                classify_probe(false, stderr),
                ProbeClass::AuthDenied,
                "{stderr:?}"
            );
        });
    }

    #[test]
    fn a_refusal_wording_inside_ssh_debug_output_is_not_a_host_verdict() {
        let proxy_noise: String = format!(
            "{SSH_KEY_FILE_MISSING}proxy: Authentication failed\nAccess denied by a local filter\n"
        );
        assert_eq!(
            classify_probe(false, &proxy_noise),
            ProbeClass::Inconclusive,
            "generic phrases count only for the anonymous HTTPS probe"
        );
    }

    #[test]
    fn each_signing_failure_fragment_outranks_a_no_access_message_on_its_own() {
        let key_accepted: &str = "debug1: Server accepts key: /k/id_work ED25519 SHA256:abc\n";
        ["signing failed", "agent refused operation"]
            .iter()
            .for_each(|fragment| {
                let stderr: String =
                    format!("{key_accepted}{fragment}\nERROR: Repository not found.\n");
                assert_eq!(
                    classify_probe(false, &stderr),
                    ProbeClass::Inconclusive,
                    "{fragment}"
                );
            });
    }
    // S5b tests: the pure rows for the four logic functions and the warning reader. The real probes are exercised by
    // hand against every configured host and are not tested here. Tests that count probe calls use a `RefCell`: the
    // probe is a shared `Fn`, so a counter cannot be a plain `let mut`.
    use crate::sync::files::{ClonePaths, SyncState, write_state};
    use std::cell::RefCell;

    const GITHUB_HTTPS: &str = "https://github.com/HidekiAI/llm-lessons.git";
    const NOW_SECS: u64 = 1_762_000_000;
    const ONE_DAY_SECS: u64 = 86_400;

    #[test]
    fn the_anonymous_url_is_derived_from_every_remote_form_or_not_at_all() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "git@github.com:HidekiAI/llm-lessons.git",
                Some(GITHUB_HTTPS),
            ),
            (
                "ssh://git@github.com/HidekiAI/llm-lessons.git",
                Some(GITHUB_HTTPS),
            ),
            (GITHUB_HTTPS, Some(GITHUB_HTTPS)),
            // Credentials never reach the anonymous probe.
            (
                "https://hideki:token@github.com/HidekiAI/llm-lessons.git",
                Some(GITHUB_HTTPS),
            ),
            // A custom port, an ssh alias (a host with no dot) and a local path have no https form.
            ("ssh://git@github.com:2222/HidekiAI/llm-lessons.git", None),
            ("git@gh:HidekiAI/llm-lessons.git", None),
            ("/srv/git/llm-lessons.git", None),
            ("file:///srv/git/llm-lessons.git", None),
            ("", None),
        ];
        cases.iter().for_each(|(remote, expected)| {
            assert_eq!(
                https_url_of_remote(remote).as_deref(),
                *expected,
                "{remote}"
            );
        });
    }

    #[test]
    fn the_visibility_recheck_is_due_when_never_checked_or_a_full_interval_old() {
        let interval_secs: u64 = ACCESS_RECHECK_DAYS * ONE_DAY_SECS;
        let cases: &[(Option<u64>, bool, &str)] = &[
            (None, true, "never checked"),
            (Some(NOW_SECS), false, "just checked"),
            (
                Some(NOW_SECS - interval_secs + 1),
                false,
                "one second short of the interval",
            ),
            (Some(NOW_SECS - interval_secs), true, "exactly the interval"),
            (
                Some(NOW_SECS - interval_secs - 1),
                true,
                "past the interval",
            ),
            // A clock that moved back must not make every cycle probe, nor hide a check for ever.
            (
                Some(NOW_SECS + ONE_DAY_SECS),
                false,
                "a check time in the future",
            ),
        ];
        cases.iter().for_each(|(last_check, expected, why)| {
            assert_eq!(access_check_due(*last_check, NOW_SECS), *expected, "{why}");
        });
    }

    fn guard_with(other_keys: &[(&str, &str)]) -> WorkGuard {
        WorkGuard {
            work_key: PathBuf::from("/k/work"),
            other_keys: other_keys
                .iter()
                .map(|(name, key)| (name.to_string(), PathBuf::from(key)))
                .collect(),
        }
    }

    /// A probe that answers by key file, and records every request it was asked.
    fn answering_by_key<'answers>(
        answers: &'answers [(&'answers str, ProbeClass)],
        asked: &'answers RefCell<Vec<ProbeRequest>>,
    ) -> impl Fn(&ProbeRequest) -> ProbeClass + 'answers {
        move |request| {
            asked.borrow_mut().push(request.clone());
            match request {
                ProbeRequest::WithKey { key, .. } => answers
                    .iter()
                    .find(|(key_path, _)| PathBuf::from(key_path) == *key)
                    .map_or(ProbeClass::Inconclusive, |(_, class)| *class),
                ProbeRequest::Anonymous { .. } => ProbeClass::Inconclusive,
            }
        }
    }

    #[test]
    fn the_work_guard_probes_the_work_key_then_every_other_key_and_judges() {
        const WORK_REMOTE: &str = "git@gitlab.example.com:org/work.git";
        type GuardRow<'row> = (&'row [(&'row str, ProbeClass)], bool, &'row str);
        let cases: &[GuardRow] = &[
            (
                &[
                    ("/k/work", ProbeClass::Readable),
                    ("/k/home", ProbeClass::AuthDenied),
                ],
                true,
                "only the work key reads",
            ),
            (
                &[
                    ("/k/work", ProbeClass::Readable),
                    ("/k/home", ProbeClass::Readable),
                ],
                false,
                "another key reads work",
            ),
            (
                &[
                    ("/k/work", ProbeClass::Readable),
                    ("/k/home", ProbeClass::Inconclusive),
                ],
                false,
                "another key is unjudged",
            ),
            (
                &[
                    ("/k/work", ProbeClass::AuthDenied),
                    ("/k/home", ProbeClass::AuthDenied),
                ],
                false,
                "the work key cannot read",
            ),
            (
                &[
                    ("/k/work", ProbeClass::Inconclusive),
                    ("/k/home", ProbeClass::AuthDenied),
                ],
                false,
                "the work key is unjudged",
            ),
        ];
        cases.iter().for_each(|(answers, allowed, why)| {
            let asked = RefCell::new(Vec::new());
            let fn_probe = answering_by_key(answers, &asked);
            let decision =
                work_guard_decision(WORK_REMOTE, &guard_with(&[("home", "/k/home")]), &fn_probe);
            assert_eq!(
                decision == AccessDecision::Allow,
                *allowed,
                "{why}: {decision:?}"
            );
        });
        // Who was probed, in what order, against which remote.
        let asked = RefCell::new(Vec::new());
        let fn_probe = answering_by_key(
            &[
                ("/k/work", ProbeClass::Readable),
                ("/k/home", ProbeClass::AuthDenied),
                ("/k/other", ProbeClass::AuthDenied),
            ],
            &asked,
        );
        work_guard_decision(
            WORK_REMOTE,
            &guard_with(&[("home", "/k/home"), ("other", "/k/other")]),
            &fn_probe,
        );
        let expected: Vec<ProbeRequest> = ["/k/work", "/k/home", "/k/other"]
            .iter()
            .map(|key| ProbeRequest::WithKey {
                remote: WORK_REMOTE.to_string(),
                key: PathBuf::from(key),
            })
            .collect();
        assert_eq!(*asked.borrow(), expected);
    }

    #[test]
    fn a_refusal_names_the_key_that_can_read_work() {
        let asked = RefCell::new(Vec::new());
        let fn_probe = answering_by_key(
            &[
                ("/k/work", ProbeClass::Readable),
                ("/k/home", ProbeClass::Readable),
            ],
            &asked,
        );
        let decision = work_guard_decision(
            "git@h:o/w.git",
            &guard_with(&[("home", "/k/home")]),
            &fn_probe,
        );
        assert!(
            matches!(&decision, AccessDecision::Refuse(reason) if reason.contains("home")),
            "{decision:?}"
        );
    }

    #[test]
    fn a_work_guard_with_no_other_key_allows_when_the_work_key_reads() {
        let asked = RefCell::new(Vec::new());
        let fn_probe = answering_by_key(&[("/k/work", ProbeClass::Readable)], &asked);
        assert_eq!(
            work_guard_decision("git@h:o/w.git", &guard_with(&[]), &fn_probe),
            AccessDecision::Allow
        );
    }

    fn state_with(visibility: Visibility, last_check: Option<u64>) -> SyncState {
        SyncState {
            visibility,
            last_access_check_unix_secs: last_check,
            ..SyncState::default()
        }
    }

    #[test]
    fn the_visibility_step_records_the_verdict_and_the_check_time() {
        let cases: &[(ProbeClass, Visibility, Option<u64>, &str)] = &[
            (
                ProbeClass::Readable,
                Visibility::Public,
                Some(NOW_SECS),
                "anonymous read means public",
            ),
            (
                ProbeClass::AuthDenied,
                Visibility::Private,
                Some(NOW_SECS),
                "turned away means private",
            ),
            // A probe with no verdict records unknown and no new check time, so the next cycle probes again.
            (
                ProbeClass::Inconclusive,
                Visibility::Unknown,
                None,
                "no verdict",
            ),
        ];
        cases
            .iter()
            .for_each(|(answer, visibility, checked_at, why)| {
                let asked = RefCell::new(Vec::new());
                let fn_probe = |request: &ProbeRequest| {
                    asked.borrow_mut().push(request.clone());
                    *answer
                };
                let state =
                    visibility_after_probe(SyncState::default(), GITHUB_HTTPS, &fn_probe, NOW_SECS);
                assert_eq!(state, state_with(*visibility, *checked_at), "{why}");
                assert_eq!(
                    *asked.borrow(),
                    vec![ProbeRequest::Anonymous {
                        url: GITHUB_HTTPS.to_string()
                    }],
                    "{why}"
                );
            });
    }

    #[test]
    fn the_visibility_step_does_not_probe_when_not_due_or_with_no_url() {
        let recent = state_with(Visibility::Public, Some(NOW_SECS - ONE_DAY_SECS));
        let asked = RefCell::new(Vec::new());
        let fn_probe = |request: &ProbeRequest| {
            asked.borrow_mut().push(request.clone());
            ProbeClass::AuthDenied
        };
        assert_eq!(
            visibility_after_probe(recent.clone(), GITHUB_HTTPS, &fn_probe, NOW_SECS),
            recent,
            "not due"
        );
        assert_eq!(
            visibility_after_probe(
                SyncState::default(),
                "/srv/git/local.git",
                &fn_probe,
                NOW_SECS
            ),
            SyncState::default(),
            "a remote with no https form is never probed"
        );
        assert!(asked.borrow().is_empty(), "no probe was run");
    }

    #[test]
    fn a_due_recheck_replaces_an_old_verdict_and_a_failed_one_keeps_the_old_verdict_and_time() {
        let old = state_with(
            Visibility::Private,
            Some(NOW_SECS - ACCESS_RECHECK_DAYS * ONE_DAY_SECS),
        );
        let went_public = visibility_after_probe(
            old.clone(),
            GITHUB_HTTPS,
            &|_: &ProbeRequest| ProbeClass::Readable,
            NOW_SECS,
        );
        assert_eq!(
            went_public,
            state_with(Visibility::Public, Some(NOW_SECS)),
            "a repo made public later is caught"
        );
        let unjudged = visibility_after_probe(
            old.clone(),
            GITHUB_HTTPS,
            &|_: &ProbeRequest| ProbeClass::Inconclusive,
            NOW_SECS,
        );
        // A flaky network at recheck time must not silently drop a known-public warning.
        assert_eq!(
            unjudged, old,
            "an unjudged recheck keeps the old verdict and its check time, so the next cycle probes again"
        );
        let public_then_unjudged = visibility_after_probe(
            state_with(
                Visibility::Public,
                Some(NOW_SECS - ACCESS_RECHECK_DAYS * ONE_DAY_SECS),
            ),
            GITHUB_HTTPS,
            &|_: &ProbeRequest| ProbeClass::Inconclusive,
            NOW_SECS,
        );
        assert_eq!(
            public_then_unjudged.visibility,
            Visibility::Public,
            "a known-public remote stays warned about"
        );
    }

    fn clone_spec(remote: &str, domains: Vec<crate::db::Domain>) -> CloneSpec {
        CloneSpec {
            remote: remote.to_string(),
            normalized_remote: remote.to_string(),
            ssh_key: PathBuf::from("/k/any"),
            domains,
        }
    }

    #[test]
    fn only_a_clone_recorded_public_is_warned_about_and_credentials_stay_out() {
        let temp_dir = tempfile::tempdir().unwrap();
        let public_clone = CloneSpec {
            // The state file is named after the NORMALIZED remote; the warning shows the remote as written, minus credentials.
            normalized_remote: "github.com/o/public".to_string(),
            ..clone_spec(
                "https://hideki:secret@github.com/o/public.git",
                vec![crate::db::Domain::Shared],
            )
        };
        let private_clone = clone_spec(
            "git@github.com:o/private.git",
            vec![crate::db::Domain::Work],
        );
        let unknown_clone = clone_spec(
            "git@github.com:o/unknown.git",
            vec![crate::db::Domain::Personal],
        );
        [
            (&public_clone, Visibility::Public),
            (&private_clone, Visibility::Private),
        ]
        .iter()
        .for_each(|(clone, visibility)| {
            let state_path =
                ClonePaths::new(temp_dir.path(), &clone.normalized_remote).state_file();
            write_state(&state_path, &state_with(*visibility, Some(NOW_SECS))).unwrap();
        });
        let warnings = public_warnings(
            temp_dir.path(),
            &[public_clone, private_clone, unknown_clone],
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("github.com/o/public.git"),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("PUBLIC"), "{warnings:?}");
        assert!(
            !warnings[0].contains("secret"),
            "credentials never reach the line: {warnings:?}"
        );
    }

    #[test]
    fn no_clone_means_no_warning() {
        let temp_dir = tempfile::tempdir().unwrap();
        assert!(public_warnings(temp_dir.path(), &[]).is_empty());
    }
    #[test]
    fn the_anonymous_url_refuses_a_slash_in_the_host_and_an_empty_path() {
        let cases: &[(&str, Option<&str>)] = &[
            // A local path with a colon in it is not an scp-like remote.
            ("/home/u.x/a:b/r", None),
            ("./rel.dir/a:b/r", None),
            // No path at all, in both spellings.
            ("git@github.com:", None),
            ("https://github.com/", None),
            ("ssh://git@github.com/", None),
        ];
        cases.iter().for_each(|(remote, expected)| {
            assert_eq!(
                https_url_of_remote(remote).as_deref(),
                *expected,
                "{remote}"
            );
        });
    }

    fn env_value<'env>(command: &'env ProbeCommand, name: &str) -> Option<&'env str> {
        command
            .env
            .iter()
            .find(|(env_name, _)| env_name == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn a_key_probe_runs_ssh_verbose_with_only_that_key_and_a_connect_timeout() {
        let command: ProbeCommand = probe_command_of(
            &ProbeRequest::WithKey {
                remote: "git@github.com:o/r.git".to_string(),
                key: PathBuf::from("/k/work"),
            },
            Some("/run/agent.sock"),
        );
        let ssh: &str = &command.ssh_command;
        let expected_parts: Vec<String> = vec![
            "-v".to_string(),
            "/k/work".to_string(),
            "IdentitiesOnly=yes".to_string(),
            "BatchMode=yes".to_string(),
            format!("ConnectTimeout={PROBE_TIMEOUT_SECS}"),
            "IdentityAgent=".to_string(),
            "-F /dev/null".to_string(),
        ];
        expected_parts
            .iter()
            .for_each(|part| assert!(ssh.contains(part.as_str()), "{part} missing from {ssh}"));
        assert_eq!(
            command.git_args,
            vec!["ls-remote", "--", "git@github.com:o/r.git", "HEAD"]
        );
        // The work-key probe must really use the key: no global or system git config (an `insteadOf` there could send it
        // to another URL read with other credentials) and nothing injected through the inherited environment. HOME stays
        // set: ssh needs it for known_hosts.
        assert_eq!(env_value(&command, "GIT_CONFIG_GLOBAL"), Some("/dev/null"));
        assert_eq!(env_value(&command, "GIT_CONFIG_NOSYSTEM"), Some("1"));
        assert_eq!(env_value(&command, "HOME"), None);
        assert_eq!(
            command.env_remove,
            vec!["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"],
            "config injected through the inherited environment never reaches the key probe"
        );
    }

    #[test]
    fn an_anonymous_probe_ignores_every_login_the_user_has_and_cannot_hang() {
        let command: ProbeCommand = probe_command_of(
            &ProbeRequest::Anonymous {
                url: GITHUB_HTTPS.to_string(),
            },
            None,
        );
        // No stored login can make a private repository look public: no askpass, no global or system git config (an
        // `insteadOf` there could turn this into an ssh read), no home directory for a netrc, no credential helper,
        // and no redirect to somewhere else.
        assert_eq!(env_value(&command, "GIT_ASKPASS"), Some(""));
        assert_eq!(env_value(&command, "GIT_CONFIG_GLOBAL"), Some("/dev/null"));
        assert_eq!(env_value(&command, "GIT_CONFIG_NOSYSTEM"), Some("1"));
        assert_eq!(env_value(&command, "HOME"), Some("/dev/null"));
        assert_eq!(env_value(&command, "XDG_CONFIG_HOME"), Some("/dev/null"));
        // Config injected through the inherited environment, and any proxy (a proxy's "access denied" page must not
        // read as a private repository), never reach the probe.
        assert_eq!(
            command.env_remove,
            vec![
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_PARAMETERS",
                "http_proxy",
                "https_proxy",
                "all_proxy",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
            ],
        );
        let expected_git_args: Vec<&str> = vec![
            "-c",
            "credential.helper=",
            "-c",
            "http.followRedirects=false",
            "ls-remote",
            "--",
            GITHUB_HTTPS,
            "HEAD",
        ];
        assert_eq!(command.git_args, expected_git_args);
        // A stalled transfer ends: git's low-speed limit and time.
        assert_eq!(
            env_value(&command, "GIT_HTTP_LOW_SPEED_LIMIT"),
            Some(PROBE_LOW_SPEED_BYTES_PER_SEC.to_string().as_str())
        );
        assert_eq!(
            env_value(&command, "GIT_HTTP_LOW_SPEED_TIME"),
            Some(PROBE_TIMEOUT_SECS.to_string().as_str())
        );
    }
    #[test]
    fn a_runner_sets_the_probe_environment_then_removes_what_must_not_be_inherited() {
        // Set and removed in one command: removal runs after setting, so the variable never reaches git.
        let command = ProbeCommand {
            ssh_command: "ssh".to_string(),
            env: vec![
                ("LESSONS_KEPT".to_string(), "kept".to_string()),
                ("LESSONS_DROPPED".to_string(), "dropped".to_string()),
            ],
            env_remove: vec!["LESSONS_DROPPED".to_string()],
            git_args: Vec::new(),
        };
        let show_canary: Vec<&str> = vec![
            "-c",
            "alias.canary=!echo \"[$LESSONS_KEPT][$LESSONS_DROPPED]\"",
            "canary",
        ];
        let shown: String = runner_for(&command).run_checked(&show_canary).unwrap();
        assert_eq!(shown.trim(), "[kept][]");
    }
}
