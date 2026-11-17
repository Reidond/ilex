use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Replay the SHA256 bank of a PC Client TPM2 binary event log, used by PR17
/// and later policy tests. EV_NO_ACTION is deliberately not extended.
#[derive(Debug, Serialize)]
pub struct Pcr7Replay {
    pub sha256: String,
    pub events: usize,
    pub policy_events: usize,
    pub authority_events: usize,
}
pub fn replay_pcr7(log: &[u8]) -> Result<Pcr7Replay> {
    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
    }
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            let end = self.at.checked_add(n).context("event log overflow")?;
            let out = self
                .bytes
                .get(self.at..end)
                .context("truncated event log")?;
            self.at = end;
            Ok(out)
        }
        fn u32(&mut self) -> Result<u32> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
        }
        fn u16(&mut self) -> Result<u16> {
            Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
        }
    }
    ensure!(log.len() <= 16 * 1024 * 1024, "event log exceeds 16 MiB");
    let mut r = Reader { bytes: log, at: 0 };
    ensure!(
        r.u32()? == 0 && r.u32()? == 3,
        "missing TCG Spec ID EV_NO_ACTION"
    );
    r.take(20)?;
    let n = r.u32()? as usize;
    let mut spec = Reader {
        bytes: r.take(n)?,
        at: 0,
    };
    ensure!(
        spec.take(16)? == b"Spec ID Event03\0",
        "unsupported TPM event log format"
    );
    spec.take(8)?;
    let count = spec.u32()?;
    ensure!(count <= 64, "too many digest algorithms");
    let mut algorithms = std::collections::BTreeMap::new();
    for _ in 0..count {
        let id = spec.u16()?;
        let size = spec.u16()? as usize;
        ensure!(
            size > 0 && size <= 128 && algorithms.insert(id, size).is_none(),
            "invalid/duplicate algorithm"
        );
    }
    ensure!(
        algorithms.get(&0x000b) == Some(&32),
        "SHA256 bank missing from event log"
    );
    let mut pcr = vec![0u8; 32];
    let (mut events, mut policy_events, mut authority_events) = (0, 0, 0);
    while r.at < log.len() {
        let index = r.u32()?;
        let kind = r.u32()?;
        let count = r.u32()?;
        ensure!(count <= algorithms.len() as u32, "invalid digest count");
        let mut sha256 = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..count {
            let alg = r.u16()?;
            ensure!(seen.insert(alg), "duplicate event digest");
            let size = *algorithms.get(&alg).context("unknown digest algorithm")?;
            let digest = r.take(size)?;
            if alg == 0x000b {
                sha256 = Some(digest)
            }
        }
        let size = r.u32()? as usize;
        r.take(size)?;
        if index == 7 && kind != 3 {
            let digest = sha256.context("PCR7 event has no SHA256 digest")?;
            pcr = Sha256::digest([pcr.as_slice(), digest].concat()).to_vec();
            events += 1;
            if kind == 0x80000001 {
                policy_events += 1
            }
            if kind == 0x800000e0 {
                authority_events += 1
            }
        }
    }
    ensure!(
        events >= 4 && policy_events > 0 && authority_events > 0 && pcr.iter().any(|b| *b != 0),
        "PCR7 lacks meaningful policy/authority measurements"
    );
    Ok(Pcr7Replay {
        sha256: pcr.iter().map(|b| format!("{b:02x}")).collect(),
        events,
        policy_events,
        authority_events,
    })
}
