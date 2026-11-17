//! Finite request frames for source chunks and detached builder jobs.
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct TransferChunk<'a> {
    pub sha256: &'a str,
    pub bytes: usize,
}

#[derive(Debug, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TransferOperation<'a, Job> {
    Missing {
        chunks: &'a [String],
    },
    PutMany {
        chunks: &'a [TransferChunk<'a>],
    },
    Submit {
        request: &'a Job,
    },
    Status {
        owner: &'a str,
        job: &'a str,
    },
    Cancel {
        owner: &'a str,
        job: &'a str,
    },
    Fetch {
        owner: &'a str,
        job: &'a str,
        path: &'a str,
        offset: u64,
    },
}

#[derive(Debug, Serialize)]
pub struct TransferFrame<'a, Job> {
    pub schema: u32,
    pub payload_bytes: usize,
    #[serde(flatten)]
    pub operation: TransferOperation<'a, Job>,
}
