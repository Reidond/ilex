# T3 validation evidence

Validation host: Windows x86_64, 2026-11-17. Base:
`tooling/02-runtime-builder`, initially at
`da9515464945f97bed116b08afabd48917b2c26b`, rebased with `gh stack rebase`
onto `436a66efeabb32fa18f7e8967dc8d3d26af2a276` when T2 moved.
Owned builder UUID: `b521170d-a2e0-4791-8b3e-71819084203d`.

## Completed checks

- Native Windows `cargo check -p xtask` and clippy for xtask/core/windows with
  `--all-targets -- -D warnings` passed during implementation.
- The corresponding Linux clippy command passed in the existing WSL environment
  with a task-specific target directory. No distro/profile change was made.
- Full `cargo clippy --workspace --all-targets -- -D warnings` passed on Windows
  and Linux. The existing Linux `doctor --self-check-build --json` also passed
  real Podman builds, source-race rejection, accepted-image preservation and
  foreign-owner rejection. Its disposable fixtures were cleaned up.
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
- A credential-named `.npmrc` was rejected before copying. After materializing
  each checkout, adding an ignored `target/ignored-input` made the real Linux
  `info --json` command fail source verification; removing it restored the
  snapshot. Cached snapshots cannot admit new ignored input silently.
- An explicit `security::firmware` filter failed natively with the T5 harness
  diagnostic before starting a job.
- Two independently materialized checkouts were inspected by the Linux xtask
  binary with their source contexts. Each returned its exact original Windows
  identity and digest; their image references and state paths differed. Neither
  acquired `main`. This validates context propagation, not concurrent VM jobs.
- On clean head `394213525a5d9f6462001b2273acf9b8c755946c`, native snapshot
  inspection recorded 1,460,299 source bytes in 189 content chunks, with digest
  `sha256:01bd554dbe7a877665a2b6d397d157b6c874051060ac8ec85ec089a4e17d9eee`.
  Two local staging runs took 2.874 and 1.940 seconds. These are local snapshot
  timings, not SSH transfer/build timings or a builder-versus-WSL comparison.
- The WSL compatibility path built this checkout's default GNOME image twice.
  Its owned tag was
  `localhost/ilex:wt-t3-6df5d392-09--5e71b6a1e2f3ac8624d5f377beb67a3b`;
  the first accepted image ID was
  `adf3ffaba430453dc776859ea81f8a608bcc48857bca7827f3cf7b226845c4e7`.
  The first invocation took 161.352 seconds (process creation to final log
  write); the repeat took 59.554 seconds (wall clock). Shared WSL caches were
  retained, other sessions were active and nothing was pruned. This is a
  checkout-first/warm baseline, not a cold-store benchmark or a speed ratio.
- WSL `cargo xtask test ephemeral:: -- --test-threads 1` passed all nine tests
  against that checkout's image: 97.28 seconds in the harness, 106.486 seconds
  for the command. This is Linux/WSL regression evidence; it does not qualify
  the Hyper-V executor.
- The Linux remote worker exported the owned image without a registry. The
  2,196,876,800-byte OCI archive's manifest named the exact image configuration
  `adf3ffaba430453dc776859ea81f8a608bcc48857bca7827f3cf7b226845c4e7`;
  its archive hash was
  `d65ed32939561ae6dcbe2fc86bc426f910c8d3338511d7bdb2da0606f0b195c6`.
  The task's temporary export/materialization directory was removed. This
  verifies export generation, not pinned-SSH delivery.

## Builder admission and remaining gates

T2 left the exact owned builder off at 4 GiB/four CPUs. The tooling's `builder
up --memory-gib 16 --cpus 8 --wait-for-memory-seconds 10800` configured the test
profile and waited for capacity. The subsequent recipe update also waits for
30 GiB available (16 GiB fixed allocation plus the unchanged 14 GiB reserve).
Available host memory during implementation was approximately 14–21 GiB. No
other VM was stopped or changed and the reserve was not reduced.

The full owned-builder build/rechunk/disk/tool/ISO/tier matrix, live Ctrl-C and
dropped-SSH recovery, concurrent checkout jobs, cache-hit evidence and cold/warm
WSL comparison remain merge gates until the builder can be admitted. Local
materialization and compile checks do not establish these results. Source and
upload measurements are emitted by the implementation; no unmeasured speed
ratio or successful tier run is claimed here.

At 16:15 UTC `publish.yml` run `44118969608` on
`phase0/17-security-probes` was in progress. No competing branch workflow was
dispatched at that point. It later completed, leaving the queue clear. The
existing workflow stages candidate images in ghcr even for branch dispatches.
The owner subsequently clarified that exactly one normal branch CI staging run
is authorized; the no-push rule applies to local/dev/builder execution.
