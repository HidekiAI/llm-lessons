//! The pure decisions of the access guards (plan step S5a): what a probe's stderr means, whether `work` may sync given
//! what every configured key can read (R6), and what the visibility probe records and warns about (R13). Nothing here
//! runs git or ssh or reads a clock; the probes that feed these functions come in S5b.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::files::{ClonePaths, SyncState, read_state};
use super::git::{GitRunner, redact_credentials, ssh_command};
use super::run::identity_agent_of;
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

/// What the last visibility probe found, recorded in the clone's state file by S5b.
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

// Table tests over the pure decisions. Each row is a case from the plan's Guards and Public warning rows, and each
// guard was shown red by removing its branch (recorded in the step result).
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
    // TODO(human): begin block 1/6
    // Credentials are stripped, and a custom port or a host with no dot is refused: the anonymous probe must not carry a
    // secret, and an ssh alias or a port names no public host.
    // let fn_host_and_path = |authority: &str, path: &str| -> Option<String> {
    //     let host: &str = authority
    //         .rsplit_once('@')
    //         .map_or(authority, |(_credentials, host)| host);
    //     (host.contains('.') && !host.contains(':') && !path.is_empty())
    //         .then(|| format!("https://{host}/{}", path.trim_start_matches('/')))
    // };
    // match remote.split_once("://") {
    //     Some(("https" | "ssh", rest)) => rest
    //         .split_once('/')
    //         .and_then(|(authority, path)| fn_host_and_path(authority, path)),
    //     Some(_) => None,
    //     None => remote
    //         .split_once(':')
    //         .and_then(|(authority, path)| fn_host_and_path(authority, path)),
    // }
    // TODO(human): end block 1/6
}

/// Whether the visibility is due for a probe: never checked, or the last check is `ACCESS_RECHECK_DAYS` old or more.
pub fn access_check_due(last_check_unix_secs: Option<u64>, now_secs: u64) -> bool {
    // TODO(human): begin block 2/6
    // An interval old or older is due; a check time in the future (a clock that moved back) is not, so a wrong clock cannot
    // make every cycle probe.
    // last_check_unix_secs.is_none_or(|last_check| {
    //     now_secs.saturating_sub(last_check) >= ACCESS_RECHECK_DAYS * SECS_PER_DAY
    // })
    // TODO(human): end block 2/6
}

/// The real probe: `git ls-remote` as the request says, its exit status and stderr read by `classify_probe`. Exercised
/// by hand against every configured host (S5), never by the tests.
pub fn probe_remote(request: &ProbeRequest) -> ProbeClass {
    // TODO(human): begin block 3/6
    // Every outcome becomes a class: a git that cannot start is Inconclusive, never a pass. ssh runs with -v because
    // classify_probe reads its debug lines; the anonymous probe disables credential helpers and prompts, so no stored login
    // can make a private repository look public.
    // let fn_class_of = |runner: &GitRunner, git_args: &[&str]| -> ProbeClass {
    //     match runner.run(git_args) {
    //         Ok(output) => classify_probe(output.exit_ok, &output.stderr),
    //         Err(spawn_error) => {
    //             journal!("probe: git did not run: {spawn_error:#}");
    //             ProbeClass::Inconclusive
    //         }
    //     }
    // };
    // match request {
    //     ProbeRequest::WithKey { remote, key } => {
    //         let probing_ssh: String = format!(
    //             "{} -v",
    //             ssh_command(key, identity_agent_of(remote).as_deref())
    //         );
    //         fn_class_of(
    //             &GitRunner::new(&std::env::temp_dir(), &probing_ssh),
    //             &["ls-remote", "--", remote, "HEAD"],
    //         )
    //     }
    //     ProbeRequest::Anonymous { url } => fn_class_of(
    //         &GitRunner::new(&std::env::temp_dir(), "ssh").with_env("GIT_ASKPASS", ""),
    //         &["-c", "credential.helper=", "ls-remote", "--", url, "HEAD"],
    //     ),
    // }
    // TODO(human): end block 3/6
}

/// The R6 decision for a `work` clone with its own remote: probe the work key, then every other key, and let
/// `decide_access` judge.
pub fn work_guard_decision(work_remote: &str, guard: &WorkGuard, probe: ProbeFn) -> AccessDecision {
    // TODO(human): begin block 4/6
    // Probe the work key first, then every other key, and let decide_access judge: the guard fails closed, so nothing here
    // may turn an unjudged key into a pass.
    // let fn_probe_with = |key: &Path| {
    //     probe(&ProbeRequest::WithKey {
    //         remote: work_remote.to_string(),
    //         key: key.to_path_buf(),
    //     })
    // };
    // let work_probe: ProbeClass = fn_probe_with(&guard.work_key);
    // let other_probes: Vec<(&str, ProbeClass)> = guard
    //     .other_keys
    //     .iter()
    //     .map(|(key_name, key)| (key_name.as_str(), fn_probe_with(key)))
    //     .collect();
    // decide_access(work_probe, &other_probes)
    // TODO(human): end block 4/6
}

/// The state after the visibility step: when a probe is due and a URL can be derived, probe anonymously and record the
/// verdict and the check time; an inconclusive probe records `unknown` and no check time (probed again next cycle).
pub fn visibility_after_probe(
    state: SyncState,
    remote: &str,
    probe: ProbeFn,
    now_secs: u64,
) -> SyncState {
    // TODO(human): begin block 5/6
    // An inconclusive probe records unknown and keeps the old check time, so the next cycle probes again; only a verdict
    // stamps the time.
    // let due_url: Option<String> = https_url_of_remote(remote)
    //     .filter(|_| access_check_due(state.last_access_check_unix_secs, now_secs));
    // match due_url {
    //     None => state,
    //     Some(url) => {
    //         let class: ProbeClass = probe(&ProbeRequest::Anonymous { url });
    //         SyncState {
    //             visibility: visibility_from_probe(class, true),
    //             last_access_check_unix_secs: (class != ProbeClass::Inconclusive)
    //                 .then_some(now_secs)
    //                 .or(state.last_access_check_unix_secs),
    //             ..state
    //         }
    //     }
    // }
    // TODO(human): end block 5/6
}

/// The PUBLIC warnings to print on stderr for the configured clones: one per clone whose recorded visibility is public,
/// read from the clones' state files (no network).
pub fn public_warnings(lessons_home: &Path, clones: &[CloneSpec]) -> Vec<String> {
    // TODO(human): begin block 6/6
    // State files only, no network: this runs before every command.
    // clones
    //     .iter()
    //     .filter_map(|clone| {
    //         let state: SyncState =
    //             read_state(&ClonePaths::new(lessons_home, &clone.normalized_remote).state_file());
    //         public_warning(&clone.remote, state.visibility)
    //     })
    //     .collect()
    // TODO(human): end block 6/6
}

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
            let probe = answering_by_key(answers, &asked);
            let decision =
                work_guard_decision(WORK_REMOTE, &guard_with(&[("home", "/k/home")]), &probe);
            assert_eq!(
                decision == AccessDecision::Allow,
                *allowed,
                "{why}: {decision:?}"
            );
        });
        // Who was probed, in what order, against which remote.
        let asked = RefCell::new(Vec::new());
        let probe = answering_by_key(
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
            &probe,
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
        let probe = answering_by_key(
            &[
                ("/k/work", ProbeClass::Readable),
                ("/k/home", ProbeClass::Readable),
            ],
            &asked,
        );
        let decision =
            work_guard_decision("git@h:o/w.git", &guard_with(&[("home", "/k/home")]), &probe);
        assert!(
            matches!(&decision, AccessDecision::Refuse(reason) if reason.contains("home")),
            "{decision:?}"
        );
    }

    #[test]
    fn a_work_guard_with_no_other_key_allows_when_the_work_key_reads() {
        let asked = RefCell::new(Vec::new());
        let probe = answering_by_key(&[("/k/work", ProbeClass::Readable)], &asked);
        assert_eq!(
            work_guard_decision("git@h:o/w.git", &guard_with(&[]), &probe),
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
                let probe = |request: &ProbeRequest| {
                    asked.borrow_mut().push(request.clone());
                    *answer
                };
                let state =
                    visibility_after_probe(SyncState::default(), GITHUB_HTTPS, &probe, NOW_SECS);
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
        let probe = |request: &ProbeRequest| {
            asked.borrow_mut().push(request.clone());
            ProbeClass::AuthDenied
        };
        assert_eq!(
            visibility_after_probe(recent.clone(), GITHUB_HTTPS, &probe, NOW_SECS),
            recent,
            "not due"
        );
        assert_eq!(
            visibility_after_probe(SyncState::default(), "/srv/git/local.git", &probe, NOW_SECS),
            SyncState::default(),
            "a remote with no https form is never probed"
        );
        assert!(asked.borrow().is_empty(), "no probe was run");
    }

    #[test]
    fn a_due_recheck_replaces_an_old_verdict_and_a_failed_one_keeps_no_stale_time() {
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
            old,
            GITHUB_HTTPS,
            &|_: &ProbeRequest| ProbeClass::Inconclusive,
            NOW_SECS,
        );
        assert_eq!(
            unjudged.visibility,
            Visibility::Unknown,
            "an unjudged recheck forgets the old verdict"
        );
        assert_eq!(
            unjudged.last_access_check_unix_secs,
            Some(NOW_SECS - ACCESS_RECHECK_DAYS * ONE_DAY_SECS),
            "and records no new check time, so the next cycle probes again"
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
        let public_clone = clone_spec(
            "https://hideki:secret@github.com/o/public.git",
            vec![crate::db::Domain::Shared],
        );
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
}
