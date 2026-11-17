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
- All 26 Rust files changed by T3 pass rustfmt with edition 2024. The full
  workspace format check still reports pre-existing formatting differences
  outside T3, including `crates/drupe/src/enter.rs`; unrelated files were retained.
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

## Builder execution and recovery

T2 left the exact owned builder off at 4 GiB/four CPUs. The tooling's `builder
up --memory-gib 16 --cpus 8 --wait-for-memory-seconds 10800` configured the test
profile and waited for capacity. The subsequent recipe update also waits for
30 GiB available (16 GiB fixed allocation plus the unchanged 14 GiB reserve).
Available host memory during the initial wait was approximately 14–21 GiB. The
builder was admitted at about 17:45 UTC after capacity became available. Its
recipe update and nested-KVM smoke check passed with 16 GiB/eight CPUs. No
other VM was stopped or changed and the reserve was not reduced.

The first actual pinned-SSH `remote self-check` passed in 68.576 seconds. Job
`46685bf1-fdf3-451d-ad1d-dae6b4706337` returned the original Windows owner
`6854065e-8793-8e65-a46c-f3bc85b2318b`, source digest
`sha256:49c58c5c171ae193f45e6c0b74cd2ab1d2c9756fcf4b335200e12b136679d78e`
and a successful nested-KVM boot. Logs, request, context and results were
delivered to Windows with hashes.

During the cold build, an owned pinned SSH process was killed after its log
response started. A subsequent status request still reported the same job
running. The Windows build client was then terminated independently; `remote
resume --job 16e0530e-f7cd-4774-b8ac-a738556f31c0` reconnected and continued
streaming from the durable log offset.

That build completed successfully: 811.539 seconds in the build operation,
0.094 seconds for the worker's warm Cargo build, and 811.893 seconds for the
Linux job including output freeze/cleanup. Builder data free space fell from
251,855,613,952 to 245,578,547,200 bytes (6,277,066,752 bytes). The resulting
image ID was `50cb268d2b04c7a68840e2245e2c920eb35a8623c71286b932e3c430c5604758`;
its owner, source digest and original `9f17529` revision labels matched the
request. This was the builder's first image build, with fresh image/build caches;
it is not comparable as a speed ratio to the retained-cache WSL baseline.

The first source upload sent 2,012,011 bytes (1,505,590 source bytes plus the
minimal HEAD pack) in 2.337 seconds. The build reused that snapshot and uploaded
zero bytes in 0.644 seconds. Snapshots took 1.943/1.828 seconds respectively.

A separate Windows console ran `cargo xtask remote self-check --hold-seconds
3600`. After its real nested-KVM smoke check, a Windows `CTRL_C_EVENT` cancelled
job `450a6ffb-7bde-439a-80db-a49b17c21379` with exit 130. The client retrieved
its result, context and logs to Windows; the exact owned builder remained
`Running`. The controlled startup-lock contention check printed its wait
diagnostic and subsequently submitted successfully. No lock error was treated
as VM absence.

While the main checkout ran/queued jobs, the spaced-path clone passed its own
pinned-SSH source/KVM self-check and headless image build. Its 274.406-second
operation produced
`localhost/ilex:wt-checkout-with--03324cfd19b648cd2b54e17d76f45d2f.s-none`, ID
`a8c442b4f73a9baf214029bb4b0e60469072285fcbaafcf45f573784094f4e7a`.
The owner was `a6335a6b-66e7-8020-9f7d-d62f435c3776`, source digest
`sha256:b2ad614519286985e4d5d72e5637e9f1201763f54ea77841a87b74b95ba9cd40`
and revision `9f17529b029977e6e9b148cdbe707e0ba657fef5-dirty`. The verified
manifest contained the staged deletion, executable bit, symlink and spaced
untracked LF input. Memory-weighted admission queued the other checkout rather
than oversubscribing this profile.

Native `cargo xtask test ephemeral:: -- --test-threads 1` passed all nine tests
in the builder, 74.71 seconds in the harness. End-to-end time was 297.638 seconds
including the preceding fixture's scheduler wait; that wall time is not a
standalone performance comparison.

The warm build operation took 17.208 seconds, with 0.093 seconds for Cargo.
Its 240.781-second Windows command included a scheduler wait behind the other
checkout. Native rechunk passed: 96.163 seconds in the operation, 128.498 seconds
end to end (including an 11.944-second Cargo rebuild for a new snapshot path).
Its fingerprint was
`sha256:16540123557d7b05f6a03a6475d8f87b393ec882f686661f931e160f3d29b2a3`.
`cargo xtask tool strace` also passed and delivered the raw tool and public
certificate to Windows. No private development key was returned.

The spaced-path fixture was then built with a dirty tracked
`rootfs/usr/libexec/ilex/update-notify` and an untracked
`rootfs/etc/ilex T3 source probe.txt`. Explicitly exporting its owned `.s-none`
variant delivered a 1,095,823,872-byte OCI archive to a Windows path with spaces.
The archive SHA-256 was
`d4da1bb17fd14284199519429195ed8e32ccc81086e19886ea8676866c767af4`;
its config digest matched image ID
`063462e7d9fd8e087a6162e7f88516b789703aeabd92ea9a79fb5eb8c06cb9bc`.
Inspecting its actual layers verified both files byte for byte against Windows,
including LF bytes. Owner and source digest matched the build receipt
(`sha256:6f6c20b36db616cf41d13904e807ee25255d602aafc9804d6c337c11f0cb6033`).
An earlier export without `--image` correctly refused the unbuilt default
variant and still returned its failure evidence.

Native `cargo xtask disk --out '<Windows path with spaces>'` passed. Installation
took 788.139 seconds; the complete command, including verified download and
explicit output copy, took 974.409 seconds. Native Windows qemu-img 11.1.3
confirmed qcow2, 32,212,254,720 virtual bytes and no backing file. Its 6,475,677,696
file/allocated bytes and independently computed Windows SHA-256 matched the
remapped `DiskArtifact`:
`11c511314ab3d9554e27cc2a7170c709df2b8cf5a57aa3fc6c6ed8c8ecb364d4`.
The content-addressed state parent and requested output were both delivered.
The existing `disk::` boot/SSH/ownership checks subsequently passed against its
builder-side parent.

A second real Ctrl-C exercise cancelled a test with a live nested container,
ID `96bd298c3e9495be2eb408a700cf099a592a3f897b14429de1105beab5e240d0`.
Job `12d84881-f755-447b-bafc-9c05b365dfeb` returned 130 and its evidence reached
Windows. Immediately retrying the same checkout/VM name passed all nine
ephemeral tests (53.43 seconds), proving its stale container/lease did not block
reuse. The builder stayed running throughout.

Live pinned-SSH requests with protocol schema 2 and a publish operation were
refused before execution. Native Windows ISO requests with `CI` and, separately,
`GITHUB_ACTIONS` set refused `--testing-tag` before creating a new job. The
existing installer table validation runs before the filtered environment is
sent to Linux; those CI flags are not forwarded.

The retained WSL image's uncompressed `.Size` was 4,986,510,961 bytes, ID
`adf3ffaba430453dc776859ea81f8a608bcc48857bca7827f3cf7b226845c4e7`, with the
same original checkout owner. Its OCI archive measurement above is independent
of the builder's headless fixture archive. Shared-store free-space changes,
uncompressed image sizes, OCI archive bytes and installed disk allocation are
different measurements; they are not interchangeable storage totals.

The first combined legacy run passed disk, graphical and all thirteen tool
tests (15 passed), but upgrade failed because libvirt could not open its console
log in the private checkout tree. All six graphical PNGs and failure logs were
delivered to Windows despite that failure. Inspection matched the partial
domain's exact UUID `836fce5d-b507-409d-92d5-954a3179cf4c`, owned source-image
digest, disk and log paths before removing that allocation. No domain XML or
embedded SSH private key was returned.

The fix routes libvirt consoles through its standard labelled log tree, then
copies them into artifact state. It also journals a per-launch nonce before
bcvk starts, so even a partial launch requires that nonce before UUID cleanup.
Windows/Linux workspace clippy and Python/Bash syntax checks passed. T2's own
update installed recipe
`89731b60d0ace71dcc6c897f5e32a5f2c9fd6cd894fa6267eb2e4b2ff2c16bb6`,
retaining its checkpoint. Admission briefly waited at 27.9 GiB available for
the unchanged 30 GiB requirement. Upgrade then passed with the warm base-disk
cache: v2 staging/boot, policy override precedence, boot-time enforcement,
signed legacy update check and rollback. The complete Windows retry took
270.202 seconds.

An actual disposable-source self-check injected a one-shot malformed resource
receipt, then its Windows client was terminated before cleanup. The completed
job returned exit 1, a cleanup error and `resource_lease_retained: true`. Before
status/resume, the real maintenance probe returned
`idle: false, reason: unreconciled_resources`. After the one-shot fault cleared,
a later status reconciled the allocation and the probe returned `idle: true`.
Native resume preserved exit 1 and retrieved the failure evidence. The fixture source was
restored afterward. This exercises the real scheduler/maintenance/error path;
no unit tests or provider mocks were added.

Native ISO build/delivery passed: 809.813 seconds in the operation and 894.476
seconds end to end. The 1,953,787,904-byte ISO and its catalogue/checksum/release
notes reached both state and an explicit Windows output path with spaces.
Independent Windows hashing matched
`4920c80ae9b2f826e583e52e7e18094bea6346f70bc7c8573ce74bd27b854452`.

The follow-up source-restoration check exposed Cargo reusing a newer worker
binary after returning to an older immutable snapshot in a shared target
directory. Targets now include the snapshot's digest/revision directory in
addition to owner/toolchain. The actual compiled-library A/B/A check passed:

| Source marker | Job | Worker compile (seconds) | Uploaded bytes |
| --- | --- | ---: | ---: |
| A | `2fc096a2-0764-4900-960c-3c6eea30d2fe` | 40.160 | 9,346 |
| B | `e06b48b5-791c-45e7-bbb8-386cb4fc38b0` | 39.715 | 1,261 |
| A again | `45229668-b549-402c-9629-97cd784030fb` | 0.085 | 0 |

The worker printed a constant compiled from the core library, and the observed
values were A, B, A. Both A runs had source digest
`sha256:745a8bc316e1f191ec79ea4af54dab85a718baac6ab3d32004ead4c883bb0812`;
B had `sha256:357fb39621edf12a79ee23da760c51984156355171ac2ae460e418294c33b88f`.
The temporary fixture edits were restored. This verifies real executable
freshness and warm reuse, rather than comparing metadata alone.

Explicit Windows output ownership was checked with two real checkout owners.
The spaced-path checkout refused the main checkout's claimed disk/archive file
and ISO directory before a new remote job was created. Positive delivery also
passed: a fresh archive path with spaces received its receipt, disk resume
delivered to a fresh claimed output, and ISO resume revalidated its existing
directory. The disk test temporarily changed only the local destination map;
the original map was restored afterward.

## Fresh-store WSL comparison

A separate WSL run used an empty, task-owned Podman graphroot/runroot and state
directory, with host kache import disabled. Tool compilation was completed
before timing. The existing shared WSL image store was retained. Both builds
used source digest
`sha256:9c235001b71353b0120abd687319d0e20ed8815e623fa9601adbb223a2c31682`
and base digest
`sha256:81ada60d0dfec088b591672bce2945d6d7ac8e32e7f263eba188bdfa809fbc04`.
The resulting image ID stayed
`da293202ee69c41c0ffae06dd9d3c831c55ec1f8c7d376ab03ee2892cf75c2cf`
for both runs; its uncompressed size was 4,986,514,545 bytes.

| Execution profile | Cold build (seconds) | Warm build (seconds) | Cold storage growth (bytes) |
| --- | ---: | ---: | ---: |
| Hyper-V builder, 16 GiB / 8 CPUs | 811.539 | 17.208 | 6,277,066,752 |
| WSL, 33,610,711,040 memory bytes / 24 CPUs | 320.448 | 54.272 | 6,332,166,144 |

Builder times are operation times from the completed job receipts; the cold
Windows client was intentionally interrupted for recovery testing. WSL times
cover the complete build command with a prebuilt xtask. Builder storage growth
is the data-volume free-space delta, including all job state; WSL growth is
allocated graphroot/runroot bytes above its 192,512-byte empty-store baseline.
The WSL warm build added 61,440 bytes. Profiles, source snapshots, measurement
boundaries and concurrent host load differ, so these observations do not
establish a controlled speed ratio. The later Windows-only destination guard
does not change the measured Linux build path.

Before removing the isolated WSL store, the cleanup verified its exact path,
owner marker, scoped storage configuration, empty container inventory and absence
of remaining mounts. Only that temporary tree was removed. No shared-store prune,
distro lifecycle or configuration operation occurred. The original shared-store
image remained `adf3ffaba430453dc776859ea81f8a608bcc48857bca7827f3cf7b226845c4e7`.

## Final builder and CI state

All heavy workload acceptance ran at 16 GiB/eight CPUs. The final compiled-cache
and output-resume checks used an 8 GiB/four-CPU profile suitable for those 4 GiB
jobs while host capacity was tight; the 14 GiB host reserve remained unchanged.
The verified recipe update was accepted and its checkpoint removed. The owned
builder was restored to 16 GiB/eight CPUs, fixed memory with nested virtualization,
and left off. Final recipe:
`056a22f21689c6b511919b8c057dea0b362fb7fe32932b9662d71996dfdfb235`.

At 16:15 UTC `publish.yml` run `44118969608` on
`phase0/17-security-probes` was in progress. No competing branch workflow was
dispatched at that point. It later completed, leaving the queue clear. The
existing workflow stages candidate images in ghcr even for branch dispatches.
The owner subsequently clarified that exactly one normal branch CI staging run
is authorized; the no-push rule applies to local/dev/builder execution.
After the workflow queue cleared, exactly one run was dispatched with tests and
ISO enabled: [44140528760](https://github.com/Reidond/ilex/actions/runs/44140528760),
at head `9f17529b029977e6e9b148cdbe707e0ba657fef5`. No further dispatch is
authorized for this task. Subsequent Windows/controller fixes are covered by the
local checks above; this run is not evidence for the final head.
The run completed successfully: build/rechunk, ephemeral and other roots,
upgrade, ISO and machine variants passed. Publishing and release jobs were
skipped as expected for a branch run.
