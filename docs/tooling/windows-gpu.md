# Native Windows development VMs

Run these commands in the checkout, in PowerShell:

```powershell
cargo ilexctl build
cargo ilexctl vm run --gui
cargo ilexctl vm run --gui --persist
cargo install --locked --path crates/ilexctl
ilexctl vm list
ilexctl vm capabilities --json
ilexctl vm self-check
ilexctl vm display
ilexctl vm screenshot -o shot.png
ilexctl vm key super
ilexctl mcp
```

`vm run --gui` asks the T3 builder for an installed development qcow2, verifies
the delivered disk manifest and SHA256, and runs the pinned T2 QEMU locally.
Image builds run on the builder. QEMU and the `ilexctl vm` controls run directly
on Windows. `vm run --disk` also uses native WHPX, with a software headless
display. Native Linux uses GTK/KVM.
On Windows, `ilexctl vm run` opens an ephemeral guest terminal on the builder.

The native profile uses q35, `-cpu max`, four vCPUs, 8 GiB fixed guest RAM,
nonsecure EDK2, virtio disk/network/keyboard and a USB absolute tablet. The
tablet keeps QEMU's absolute pointer handler active across guest shutdown,
avoiding a grabbed SDL window-title deadlock in the pinned runtime. Graphics are exactly
`-accel whpx,kernel-irqchip=off -device virtio-vga-gl -display
sdl,gl=on,grab-mod=rctrl`. The child's private environment sets
`SDL_WINDOWS_DPI_AWARENESS=permonitorv2` and
`SDL_WINDOW_ACTIVATE_WHEN_SHOWN=0`. The window opens without taking focus,
avoiding SDL's startup grab/title deadlock when the pointer is already over it.
GPU initialization failure is an
error. `--no-gl` is an explicit software troubleshooting choice. This profile
does not claim Secure Boot, a persistent TPM, accelerated Vulkan or video decode.

The base disk remains immutable. Each launch gets a writable qcow2 overlay
and its own UEFI variable store; `--persist` retains that pair for subsequent
launches of the same installed base. Closing SDL exits QEMU. Ctrl-C stops only
this launch, requesting guest poweroff before an owned-process deadline.
QEMU runs without the launcher's console so a physical console Ctrl-C reaches
the launcher instead of terminating QEMU before that request. Normal teardown
records poweroff acknowledgement, deadline fallback and exit code in
`teardown-status.json`.
The Windows Job Object also contains the launch's helper processes. Logs and
metadata remain in the checkout's runtime directory; ordinary overlays and
run-specific access keys are removed after normal teardown. Forced launcher
termination closes its Job Object and kills QEMU; the next launch of that
owned purpose removes the dead run's keys, private sockets and ephemeral disk.
Persistent disks remain
under the checkout's state directory.

`--purpose research` selects a separate owned overlay and control endpoint.
An explicit `ILEX_DISK` accepts a validated development disk from Linux:
run `cargo ilexctl disk --development`, then copy the qcow2, `.disk.json` manifest
and `.ilex-owner` receipt together. Import requires this checkout's owner and
the exact SHA256 and preserves the original executor in image provenance.
The normal builder delivery path remains the default.
`remote resume --job <id>` reuses a completed development-disk collection,
validates its checkout/job/source identity and checksum, and finishes local
promotion without another transfer or builder startup.

Admission waits for guest RAM plus a 14 GiB host reserve and a native VM slot
(`ILEX_VM_SLOTS`, default four). Live available memory already includes the
fixed builder and other applications; nested builder RAM is not counted
again. Builder and native startup share a byte-range lock; native guest RAM
is preallocated before that lock is released. Resource sizes are never reduced
to get past admission. The shared
builder's supported `builder down` refuses active jobs and retains its disks.

## Input and resolution

Click in SDL to grab input. Super/Windows combinations, Alt+Tab and Ctrl+Esc
belong to the guest while grabbed. **Right-Ctrl+G** releases/reacquires the
grab. Losing focus releases held keys. Ctrl+Alt+Del remains host-reserved.
The qualified SDL runtime needs no custom keyboard hook.

SDL advertises physical client dimensions through virtio EDID. The session
observer debounces resize, checks the actual compositor mode over pinned SSH,
and records `resize-status.json`. Each SSH transaction has a five-second deadline;
probing continues every two seconds after success or failure, including long
periods at GDM. Session identity, compositor instance, connector EDID hashes
and outputs are observed again, so login, re-login and compositor changes
recover without another host resize. SDL also
[resizes its window when a guest surface changes](https://github.com/qemu/qemu/blob/v11.1.3/ui/sdl2.c#L144),
including a compositor's bootstrap mode. The observer retains the last
acknowledged host target when the guest-driven window change leaves EDID
unchanged, allows two seconds for a human resize's EDID update, then restores
that target through a second bounded transaction. Recovery counters and the
last corrected dimensions remain in the status file. GNOME follows its preferred mode. The
Hyprland adapter selects an advertised mode through `hyprctl` when needed.
If its mode list is stale, the adapter can use the exact preferred timing
from the matching DRM connector's checked EDID; it does not synthesize modes.
Current Hyprland uses Lua runtime commands, with the older keyword fallback;
it never writes monitor configuration. An unsupported mode stays unacknowledged.
`ilexctl vm display` reports the actual guest mode independently of screenshot
size. Screenshot/input coordinates on Windows are cropped **client pixels**;
QMP absolute coordinates are normalized against that viewport. Capture
generation changes with the viewport and stale MCP mappings are rejected.
vmctl rejects pointer input until the actual guest mode matches client pixels,
so transient SDL letterboxing cannot send a click to the wrong location.

## Development clipboard and SSH

`disk --development` adds `dev-session/` with the same collision-checking
overlay installer used for production composition. It does not add a machine
to `image.toml`, run a package transaction over shell overlays or change
production files. The `ilex.development` image label is rejected by every
publication path, including staged-digest signing. GNOME supplies the Python
GObject bindings; a Hyprland research composition also needs `wl-clipboard`.
Only that development layer contains the session and host-key provisioning
helpers. No clipboard helper ships in published ilex images.
After pinned SSH is ready, the launcher refreshes the session adapter in the
disposable guest's `/run/ilex-dev-session`, records its source hash and starts
it automatically. The guest host key stays in its separate root-only directory.

Each launch generates an SSH access key and a guest host key in a directory
with current-user/SYSTEM ACLs. The guest receives its host identity and public
access credential through the owned QEMU firmware channel. Its volatile
authorized-key file is refreshed on every native boot, including persistent
overlays. The SMBIOS credential remains available to the earlier installation
path. Systemd's normal credential setup preserves existing authorized keys,
so it cannot refresh a persistent guest's launch key by itself (see
[systemd credentials](https://www.freedesktop.org/software/systemd/man/latest/systemd.system-credentials.html)).
SSH requires that exact host key;
neither first-contact acceptance nor disabled host verification is used.
Run-specific host identity is independent of the persistent disk's identity.
The earlier `bcvk to-disk` installer boot has no native firmware identity and
uses a volatile `/run` host key. Native clients still require the launch's
exact firmware-delivered key, so that installer key cannot satisfy their pin.

The native Windows `AddClipboardFormatListener` and the active guest user's
helper exchange length-framed UTF-8 JSON over that SSH connection. Text is
limited to 256 KiB, messages and queues are bounded, transfers have deadlines,
and origins, monotonic sequences and session generations suppress echoes.
Contents and transport exceptions are never logged. `session-status.json`
contains only activity and counters.

The root supervisor selects only an active local unlocked Wayland **user**
session, launches clipboard access as that user, and tears it down at lock,
logout, SSH EOF or VM exit. It reconnects with a new generation after login
or unlock. GDM is excluded. GNOME 51 uses the spike-qualified **private**
Mutter RemoteDesktop clipboard API, with version and method probes; this is
not advertised as a stable public GNOME API. Hyprland uses its Wayland
data-control route. Unsupported compositors remain unavailable. No SPICE or
qemu-vdagent clipboard bridge is involved.

## Control and capture

Private `vm.json` identifies owner/run, PID creation time, exact HWND, runtime,
disk and endpoints. Control and human-input QMP endpoints are independent
Windows AF_UNIX sockets in a new per-launch directory. Protected NTFS DACLs
allow only the current user and SYSTEM before QEMU binds. On every launch,
the verifier requires the socket owner to equal the process TokenUser and
exactly two full-access allow ACEs for that user and SYSTEM, with no propagation
flags or other entries. Unexpected or unreadable security aborts startup before
vm.json is published. Each client opens no-follow directory/file handles,
repeats the owner/ACL and native socket reparse-tag checks, and retains those
handles while connected. TCP endpoints
are rejected; there is no debug TCP opt-in. AF_UNIX uses QEMU's reconnectable
socket backend; the pinned QEMU pipe backend cannot reconnect clients.
[Windows AF_UNIX permission checks](https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/)
enforce the socket file's ACL at connect time. vmctl commands serialize
the control connection, correlate QMP IDs, ignore asynchronous events and
enforce absolute deadlines. `qmp-security.json` records the owner and every
verified ACE before vm.json is written.
The zero-guest `qmp-self-check` e2e example and `windows-qmp-security.py`
exercise reconnects and raw connections from same-user low-integrity child
tokens; both must be refused by the OS without production ACL preflight.
The self-check also applies invalid DACLs to the stopped fixture's sockets and
requires the verifier to reject them or record Windows' safe normalization.
No cross-user live test or new Windows account is used; the orchestrator
accepted the enumerated owner/DACL and mandatory startup enforcement as the
substitute evidence. See the [raw evidence](windows-gpu-evidence.md).
The firmware-only runtime activation probe uses this transport too. Its
unchanged 256 MiB profile preallocates RAM while holding the shared host
admission lock. Socket names use Windows' OS-known per-user Local AppData
directory, so long isolated artifact-state paths cannot exceed `sockaddr_un`.

Virgl screenshots use Windows Graphics Capture `CreateForWindow` for that
owned HWND, cropped with physical DWM/client geometry. Every acquisition
creates a new pool, validates dimensions and requires a new compositor QPC
timestamp. Capture never foregrounds, restores or captures the desktop.
Resize invalidates an in-flight frame. Minimized/empty/stale acquisitions fail
with a reason instead of returning an old PNG. A non-minimized PrintWindow
fallback has a deadline, geometry/nonblack validation and requires changed
client pixels after its baseline acquisition. Static, black or unchanged
PrintWindow output fails with `no fresh frame`. Successful captures record
both content hashes and observation times, use `printwindow-content-change`
as their source and leave compositor ticks null. Observation time is never
used as proof of presentation. At most one fallback worker can be outstanding.
`screenshot` writes a `.frame.json` sidecar with identity, freshness proof,
timestamp, source, dimensions and generation. `wait-screen` uses fresh frames;
equal pixels alone do not establish freshness.

## Acceptance

The scripts in `dev-session/checks/` are real-image probes, not unit tests,
and are not copied into the development image. `windows-e2e.py` authenticates
the pinned endpoint, provisions a disposable `ilexdev` user, runs an independent
Wayland GL/clipboard client and records delivered guest evdev events. Its
input mode refuses to inject unless Cua has selected the exact foreground
QEMU HWND. Injection uses physical `SendInput` scan codes with extended flags.
`qt-motion.qml` records Qt `frameSwapped` intervals over a sustained workload;
those are application callback intervals, not input-to-photon measurements.
`windows-mcp.py` exercises the real stdio MCP server and native captures. With
`--resize`, it waits for a Cua window resize, checks that old screenshot
coordinates are rejected, and recovers by acquiring a new screenshot.
`windows-resize-recovery.py` keeps GDM stopped beyond the former retry window,
then verifies delayed login and session changes at unchanged host dimensions.
For the compositor regression, stage the disposable research guest with
`hyprland-research.sh --disposable-research-vm --stage-only --small-bootstrap-mode`
while GNOME is acknowledged at a larger window size. Run the session fixture
with `--backend hyprland --expect-guest-mode-recovery session-change`, then
repeat it for re-login without touching the host window.

T4 merge acceptance requires the actual ilex GNOME image and the separate
Fedora Hyprland/Qt research workload, renderer evidence, sustained motion and
resize, real host keyboard/escape/focus release, Unicode clipboard with
re-login/lock and loop checks, and fresh cropped captures across resize,
occlusion and minimize/restore. The PR records commands, versions, resources,
frame rates, input provenance and owned cleanup. Spike evidence alone does
not satisfy these gates.
