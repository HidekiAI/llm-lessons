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

/// Lower-case stderr fragments where the host authenticated the caller and then said no: GitHub prints "Repository not
/// found" only after authentication, and an anonymous HTTPS probe of a private repo is turned away with a prompt that
/// is disabled. Fragments for other hosts join this list only once S5 has seen them by hand.
const NO_ACCESS_MESSAGES: &[&str] = &[
    "repository not found",
    "access denied",
    "terminal prompts disabled",
    "could not read username",
    "authentication failed",
];

/// Lower-case stderr fragments of an ssh agent that holds the key but did not sign: the host never judged the key.
const SIGNING_FAILURES: &[&str] = &["signing failed", "agent refused operation"];

/// What a probe's exit status and stderr mean. The stderr of an ssh probe is `ssh -v` output, so the lines say how far
/// authentication got with THIS key: offered but never accepted means the server rejected it (`AuthDenied`); accepted
/// and then a signing failure means the host never judged it (`Inconclusive`); no offer at all (a missing or
/// passphrase-protected key, a host-key failure) is `Inconclusive` too. An unrecognised message is never `AuthDenied`.
pub fn classify_probe(exit_ok: bool, stderr: &str) -> ProbeClass {
    // Order matters: a signing failure outranks a no-access message, and anything unrecognised falls to Inconclusive,
    // so a new host's wording can never turn an unknown into a pass of the R6 guard.
    let lowered_stderr: String = stderr.to_lowercase();
    let fn_says_any = |fragments: &[&str]| {
        fragments
            .iter()
            .any(|fragment| lowered_stderr.contains(fragment))
    };
    let key_offered_but_not_accepted: bool = lowered_stderr.contains("offering public key")
        && !lowered_stderr.contains("server accepts key");
    match (
        exit_ok,
        fn_says_any(SIGNING_FAILURES),
        fn_says_any(NO_ACCESS_MESSAGES) || key_offered_but_not_accepted,
    ) {
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
            "WARNING: {} is PUBLIC: anyone can read these lessons and their history; change it in the repo's Settings > Danger Zone",
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
        let both: String =
            format!("{SSH_KEY_ACCEPTED_THEN_AGENT_REFUSED}ERROR: Repository not found.\n");
        assert_eq!(classify_probe(false, &both), ProbeClass::Inconclusive);
    }

    #[test]
    fn decide_access_refuses_unless_the_work_key_reads_and_no_other_key_can() {
        use AccessDecision::{Allow, Refuse};
        use ProbeClass::{AuthDenied, Inconclusive, Readable};
        let fn_refusal_names = |decision: &AccessDecision, key_name: &str| matches!(decision, Refuse(reason) if reason.contains(key_name));
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
            Refuse(reason) if reason.contains("work key")
        ));
        assert!(matches!(
            decide_access(Inconclusive, &[("home", AuthDenied)]),
            Refuse(reason) if reason.contains("work key")
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
}
