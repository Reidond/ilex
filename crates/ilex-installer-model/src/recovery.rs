//! The recovery key's shape and the rule that checks a person has really
//! written it down: said once, for the helper (which holds the key and is the
//! one that decides), every interface (which asks for the groups and words the
//! refusals) and the tests.
//!
//! The key is systemd's: eight groups of eight letters from a sixteen-letter
//! alphabet, joined with hyphens (`cdefghij-klnrtuvc-…`, 71 characters). It
//! is shown once, on paper-sized type, and the whole key is typed back before
//! the installation relies on it (as phase 0's local completion does): the
//! helper compares all eight groups with the key it holds and opens the key's
//! keyslot with what was typed.

use serde::{Deserialize, Serialize};

/// How many groups a key has.
pub const GROUPS: usize = 8;
/// How many characters each group has.
pub const GROUP_CHARS: usize = 8;
/// A whole key: the groups and the hyphens between them.
pub const KEY_CHARS: usize = GROUPS * GROUP_CHARS + (GROUPS - 1);
/// systemd's recovery-key alphabet: sixteen letters chosen to differ on every
/// layout and be hard to confuse on paper.
pub const ALPHABET: &str = "cbdefghijklnrtuv";

/// Is `text` a recovery key as systemd makes one (and so as the helper may
/// hand it over)?
pub fn is_key(text: &str) -> bool {
    text.len() == KEY_CHARS
        && text.split('-').count() == GROUPS
        && text
            .split('-')
            .all(|group| group.len() == GROUP_CHARS && group.chars().all(is_letter))
}

fn is_letter(c: char) -> bool {
    ALPHABET.contains(c)
}

/// The key's groups, in order (empty when it is not a key).
pub fn groups(key: &str) -> Vec<&str> {
    if is_key(key) {
        key.split('-').collect()
    } else {
        Vec::new()
    }
}

/// What a person typed for a group, reduced to what is compared: letters only,
/// lower case, no hyphens or spaces (a person copying from paper adds either).
pub fn normalize(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

/// What typing the key can be wrong about, before it is compared with the
/// helper's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyProblem {
    /// Nothing typed.
    Empty,
    /// Fewer letters than a key has: how many were typed (the groups before
    /// `letters / GROUP_CHARS + 1` are complete).
    Short { letters: usize },
    /// More letters than a key has.
    Long,
    /// A letter that no key has (the alphabet leaves out `a`, `m`, `o`, `p`,
    /// `q`, `s`, `w`, `x`, `y`, `z` and the digits), in the 1-based group it
    /// falls in.
    NotInKey { group: usize },
}

/// Is `typed` the shape of a whole key? (Whether it is the right one is the
/// helper's to say.) A person copying from paper adds hyphens, spaces and
/// capitals; they are forgiven, the groups are counted without them.
pub fn key_problem(typed: &str) -> Option<KeyProblem> {
    let letters = normalize(typed);
    if letters.is_empty() {
        return Some(KeyProblem::Empty);
    }
    if let Some(at) = letters.chars().position(|c| !is_letter(c)) {
        return Some(KeyProblem::NotInKey {
            group: (at / GROUP_CHARS + 1).min(GROUPS),
        });
    }
    match letters.chars().count().cmp(&(GROUPS * GROUP_CHARS)) {
        std::cmp::Ordering::Less => Some(KeyProblem::Short {
            letters: letters.chars().count(),
        }),
        std::cmp::Ordering::Greater => Some(KeyProblem::Long),
        std::cmp::Ordering::Equal => None,
    }
}

/// The key as the paper has it (eight hyphenated groups, lower case), from what
/// was typed: `None` unless it is a whole key's shape. What `AcknowledgeRecoveryKey`
/// carries.
pub fn canonical(typed: &str) -> Option<String> {
    if key_problem(typed).is_some() {
        return None;
    }
    let letters = normalize(typed);
    let groups: Vec<&str> = (0..GROUPS)
        .map(|i| &letters[i * GROUP_CHARS..(i + 1) * GROUP_CHARS])
        .collect();
    Some(groups.join("-"))
}

/// Does `typed` give all eight groups of `key`? Compared without stopping at
/// the first difference.
pub fn matches(key: &str, typed: &str) -> bool {
    let Some(typed) = canonical(typed) else {
        return false;
    };
    if !is_key(key) {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in key.bytes().zip(typed.bytes()) {
        diff |= a ^ b;
    }
    key.len() == typed.len() && diff == 0
}

/// Where the recovery key stands, as `State` reports it. The key itself is
/// never in a state: only whether it was handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyStatus {
    /// The helper holds a key nobody has taken yet.
    Waiting,
    /// An interface took it (it is read once): it cannot be shown again, and
    /// the helper waits for the whole key typed back.
    Shown,
    /// The whole key was typed back, matched and opened its keyslot.
    Acknowledged,
    /// The person chose to go on without a recovery key (`ContinueWithoutRecoveryKey`):
    /// the keyslot nobody saw is recorded for `ilex-enroll complete
    /// --reissue-recovery`, the security chip is not enrolled and the disk opens
    /// with its passphrase only.
    Declined,
}

/// What the TPM step ended as. Mirrors `setup.json`'s `security.tpm.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TpmStatus {
    /// The security chip unlocks the disk; the first boot that does it makes
    /// this `verified`.
    EnrolledUnverified,
    /// The disk asks for its passphrase (or the recovery key) at every start.
    PassphraseOnly,
}

/// How the security chip step ended, with the reason code `setup.json` keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TpmOutcome {
    pub status: TpmStatus,
    /// `pcr7-sha256` when enrolled; otherwise `secure-boot-off`, `no-tpm`,
    /// `pcr7-unmeasured`, `tool-mismatch`, `unlock-stack-missing`,
    /// `not-requested`, `no-recovery-key`, `recovery-key-failed` or
    /// `enrollment-failed`.
    pub reason: String,
}

/// Why the disk asks for its passphrase at every start, in a clause that follows
/// "asks for the passphrase at start:" (the done page) or "is not used:" (the
/// plan's warning). `code` is a reason code of `setup.json`.
pub fn tpm_reason(code: &str) -> String {
    match code {
        "pcr7-sha256" => "the security chip unlocks the disk".to_owned(),
        "secure-boot-off" => "Secure Boot is off, or in setup mode, on this computer".to_owned(),
        "no-tpm" => "this computer has no usable TPM 2.0 security chip".to_owned(),
        "pcr7-unmeasured" => {
            "this computer's firmware does not measure its Secure Boot state into the security chip"
                .to_owned()
        }
        "tool-mismatch" => {
            "the disk tools of the installed system are not the ones qualified for the security chip"
                .to_owned()
        }
        "unlock-stack-missing" => {
            "the installed system cannot unlock its disk with the security chip".to_owned()
        }
        "not-requested" => "automatic unlock was turned off".to_owned(),
        "no-recovery-key" => {
            "no recovery key was made, because nothing in this installation could show it"
                .to_owned()
        }
        "recovery-key-failed" => "no recovery key was made and confirmed".to_owned(),
        "enrollment-failed" => "enrolling the security chip failed".to_owned(),
        other => format!("reason code {other}"),
    }
}
