# T3 validation evidence

Validation host: Windows x86_64, 2026-11-17. Base:
`tooling/02-runtime-builder` at `da9515464945f97bed116b08afabd48917b2c26b`.
Owned builder UUID: `b521170d-a2e0-4791-8b3e-71819084203d`.

## Completed checks

- Native Windows `cargo check -p xtask` and clippy for xtask/core/windows with
  `--all-targets -- -D warnings` passed during implementation.
- The corresponding Linux clippy command passed in the existing WSL environment
  with a task-specific target directory. No distro/profile change was made.
- `remote snapshot` inventoried a real disposable Git clone named `checkout with
  spaces`, including a staged deletion of `README.md`, executable
  `build/compose.py`, an untracked spaced filename with exact LF bytes and a
  Git-mode-120000 Windows symlink file pointing to `Cargo.toml`.
- Its owner was `a6335a6b-66e7-8020-9f7d-d62f435c3776`, distinct from this T3
  checkout. Its manifest digest was
  `sha256:6c8aac9803a348b5eeb3d7c997ad78ed2f1edad7ec2ca9227b8fff7ec030c673`.
  The Linux materializer created it, verified the bytes/mode/link/deletion and
  accepted a second warm reuse. Both temporary Linux materializations were
  removed. This checks source staging, not a builder image-build acceptance run.
- CRLF in an LF-attributed untracked text file was rejected before builder
  access. An escaping symlink-file input was also rejected.
- An explicit `security::firmware` filter failed natively with the T5 harness
  diagnostic before starting a job.

## Builder admission and remaining gates

T2 left the exact owned builder off at 4 GiB/four CPUs. The tooling's `builder
up --memory-gib 16 --cpus 8 --wait-for-memory-seconds 10800` configured the test
profile and waited for capacity. The subsequent recipe update also waits for
30 GiB available (16 GiB fixed allocation plus the unchanged 14 GiB reserve).
Available host memory during implementation was approximately 14–16 GiB. No
other VM was stopped or changed and the reserve was not reduced.

The full owned-builder build/rechunk/disk/tool/ISO/tier matrix, live Ctrl-C and
dropped-SSH recovery, concurrent checkout jobs, cache-hit evidence and cold/warm
WSL comparison remain merge gates until the builder can be admitted. Local
materialization and compile checks do not establish these results. Source and
upload measurements are emitted by the implementation; no unmeasured speed
ratio or successful tier run is claimed here.

At 16:15 UTC `publish.yml` run `44118969608` on
`phase0/17-security-probes` was in progress. No competing branch workflow was
dispatched at that point.
