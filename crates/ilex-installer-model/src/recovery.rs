//! The recovery key's shape and the rule that checks a person has really
//! written it down: said once, for the helper (which holds the key and is the
//! one that decides), every interface (which asks for the groups and words the
//! refusals) and the tests.
//!
//! The key is systemd's: eight groups of eight letters from a sixteen-letter
//! alphabet, joined with hyphens (`cdefghij-klnrtuvc-…`, 71 characters). It
//! is shown once, on paper-sized type, and two of its groups are typed back
//! before the installation relies on it: the interface chooses which two, the
//! helper compares them with the key it holds.

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
/// How many groups a person is asked to type back.
pub const ASKED: usize = 2;

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

/// What typing a group can be wrong about, before it is compared with the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupProblem {
    /// Nothing typed.
    Empty,
    /// Fewer characters than a group has.
    Short,
    /// More characters than a group has.
    Long,
    /// A character that no key has (the alphabet leaves out `a`, `m`, `o`, `p`,
    /// `q`, `s`, `w`, `x`, `y`, `z` and the digits).
    NotInKey,
}

/// Is `typed` the shape of a group? (Whether it is the right one is the
/// helper's to say.)
pub fn group_problem(typed: &str) -> Option<GroupProblem> {
    let group = normalize(typed);
    if group.is_empty() {
        return Some(GroupProblem::Empty);
    }
    if !group.chars().all(is_letter) {
        return Some(GroupProblem::NotInKey);
    }
    match group.chars().count().cmp(&GROUP_CHARS) {
        std::cmp::Ordering::Less => Some(GroupProblem::Short),
        std::cmp::Ordering::Greater => Some(GroupProblem::Long),
        std::cmp::Ordering::Equal => None,
    }
}

/// Which two groups a person is asked for: one from the first half and one
/// from the second, as 1-based numbers, chosen from `seed` (any bytes: a
/// plan's id, a counter). The same seed asks for the same groups again, so a
/// retry after a typing mistake asks the same question and a restarted
/// interface asks what its predecessor did.
pub fn asked(seed: &str) -> [usize; ASKED] {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let half = GROUPS / 2;
    let first = (hash % half as u64) as usize + 1;
    let second = ((hash >> 16) % half as u64) as usize + 1 + half;
    [first, second]
}

/// What the helper's `AcknowledgeRecoveryKey` is given as its secret: one
/// line per asked group, `<number> <group as typed>`.
pub fn answer_text(answer: &[(usize, String)]) -> String {
    let mut text = String::new();
    for (number, typed) in answer {
        text.push_str(&format!("{number} {typed}\n"));
    }
    text
}

/// A parsed answer: the groups' numbers (1-based) and what was typed for each,
/// normalised.
pub fn parse_answer(text: &str) -> Result<Vec<(usize, String)>, &'static str> {
    let mut seen = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (number, typed) = line
            .trim()
            .split_once(' ')
            .ok_or("a line is not `<number> <group>`")?;
        let number: usize = number
            .parse()
            .map_err(|_| "a group number is not a number")?;
        if !(1..=GROUPS).contains(&number) {
            return Err("a group number is not between 1 and 8");
        }
        if seen.iter().any(|(n, _)| *n == number) {
            return Err("a group is given twice");
        }
        seen.push((number, normalize(typed)));
    }
    if seen.len() != ASKED {
        return Err("two groups are needed");
    }
    Ok(seen)
}

/// Do the typed groups match the key's? Compared without stopping at the first
/// difference.
pub fn matches(key: &str, answer: &[(usize, String)]) -> bool {
    let groups = groups(key);
    if groups.is_empty()
        || answer.len() != ASKED
        || (1..answer.len()).any(|i| answer[..i].iter().any(|(n, _)| *n == answer[i].0))
    {
        return false;
    }
    let mut same = true;
    for (number, typed) in answer {
        let Some(group) = number.checked_sub(1).and_then(|i| groups.get(i)) else {
            return false;
        };
        if group.len() != typed.len() {
            same = false;
            continue;
        }
        let mut diff = 0u8;
        for (a, b) in group.bytes().zip(typed.bytes()) {
            diff |= a ^ b;
        }
        same &= diff == 0;
    }
    same
}

/// Where the recovery key stands, as `State` reports it. The key itself is
/// never in a state: only whether it was handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyStatus {
    /// The helper holds a key nobody has taken yet.
    Waiting,
    /// An interface took it (it is read once): it cannot be shown again, and
    /// the helper waits for two groups of it.
    Shown,
    /// Two groups were typed back and matched.
    Acknowledged,
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
        "recovery-key-failed" => "the recovery key could not be made".to_owned(),
        "enrollment-failed" => "enrolling the security chip failed".to_owned(),
        other => format!("reason code {other}"),
    }
}
