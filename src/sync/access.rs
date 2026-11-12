//! The pure decisions of the access guards (plan step S5a): what a probe's stderr means, whether `work` may sync given
//! what every configured key can read (R6), and what the visibility probe records and warns about (R13). Nothing here
//! runs git or ssh or reads a clock; the probes that feed these functions come in S5b.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "wired into the cycle in plan step S5b")
)]

use super::git::redact_credentials;

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
}
