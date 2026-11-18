# T4 native Windows acceptance

Raw artifacts are checked in under [`evidence/t4`](evidence/t4). The
[`manifest`](evidence/t4/manifest.json) records SHA256, byte count and original
file-write UTC for every copied artifact. Capture sidecars contain acquisition
UTC, presentation ticks, run identity and physical client size. Application
frame-rate logs retain their elapsed-time samples. These are the original
probe outputs; the small `identity.json` files select public identity fields
from runtime metadata and omit credential paths.

| Acceptance claim | Raw evidence |
| --- | --- |
| Builder delivery, installed disk identity and successful job | [Receiver receipt](evidence/t4/builder-delivery/collection.json), [worker result](evidence/t4/builder-delivery/result.json), [disk manifest](evidence/t4/builder-delivery/disk.windows.json), [worker stdout](evidence/t4/builder-delivery/job/stdout.log), [worker stderr](evidence/t4/builder-delivery/job/stderr.log), [offline resume](evidence/t4/offline-resume.stdout.log). The default delivery reused the disk installed by the earlier builder job. |
| Actual GNOME renderer and final-profile sustained frame rate | [Guest facts](evidence/t4/builder-cold1/guest-facts.txt), [GTK renderer and five-second frame samples](evidence/t4/usb-gnome-pass/gnome-motion.json), [owned run identity](evidence/t4/usb-gnome/identity.json). |
| GNOME real host input and focus release | [SendInput driver result](evidence/t4/usb-gnome-input/host-input.json), [raw guest evdev events](evidence/t4/usb-gnome-input/guest-input.json), [focus assertions](evidence/t4/gnome-input/focus-loss.json), [focus evdev trace](evidence/t4/gnome-input/focus-events.json). |
| GNOME Unicode, byte limits, no loops and re-login/lock | [Unicode checks](evidence/t4/usb-gnome-pass/clipboard.json), [byte-boundary results](evidence/t4/usb-gnome-pass/byte-limits.json), [65-second counter observations](evidence/t4/usb-gnome-pass/clipboard-idle-counters.json), [re-login](evidence/t4/builder-relogin/relogin.json), [post-login Unicode](evidence/t4/builder-relogin/clipboard.json), [post-login counters](evidence/t4/builder-relogin/clipboard-idle-counters.json), [lock](evidence/t4/gnome-lock/lock.json). Payloads are not recorded. |
| Hyprland/Qt renderer, motion and resize | [Qt renderer/runtime output](evidence/t4/usb-hyprland-qt/qt-renderer-and-motion.txt), [raw frame interval statistics](evidence/t4/usb-hyprland-qt/qt-motion.json), [guest output after resize](evidence/t4/usb-hyprland-qt/display-after-resize.json), [before crop metadata](evidence/t4/usb-hyprland-qt/before-resize.frame.json), [after crop metadata](evidence/t4/usb-hyprland-qt/after-resize.frame.json), [sampled recording audit](evidence/t4/usb-hyprland-qt/recording/audit.json). |
| Hyprland host input, compositor actions and focus release | [Host driver](evidence/t4/usb-hyprland-input/host-input.json), [guest evdev](evidence/t4/usb-hyprland-input/guest-input.json), [focus result](evidence/t4/hyprland-focus/focus-loss.json), [focus evdev](evidence/t4/hyprland-focus/focus-events.json). |
| Qt Unicode and re-login without loops | [Qt checks](evidence/t4/usb-hyprland-qt/qt-clipboard.json), [65-second counters](evidence/t4/usb-hyprland-qt/qt-clipboard-idle.json), [re-login](evidence/t4/hyprland-relogin/relogin.json), [post-login checks](evidence/t4/hyprland-relogin/qt-clipboard.json), [post-login counters](evidence/t4/hyprland-relogin/qt-clipboard-idle.json). |
| WGC occlusion, minimize and restore | [Occluded frame A](evidence/t4/hyprland-relogin/occluded-a.frame.json), [frame B](evidence/t4/hyprland-relogin/occluded-b.frame.json), [observations](evidence/t4/hyprland-relogin/capture-checks.json), [verified-minimize error](evidence/t4/hyprland-minimized-verified.stderr.log), [restored crop](evidence/t4/hyprland-relogin/restored-verified.frame.json). The earlier `minimized.png` was an unsuccessful setup attempt and is excluded. |
| Native vmctl and MCP mapping recovery | [Real native self-check](evidence/t4/builder-cold1/vmctl-self-check.json), [stdio MCP resize check](evidence/t4/usb-gnome-mcp/mcp-check.json). |
| Repeated boot credentials, pin rejection and enforcing labels | [First boot](evidence/t4/builder-cold1/ssh-identity.json), [second boot](evidence/t4/builder-cold2/ssh-identity.json), [final GNOME boot](evidence/t4/usb-gnome/ssh-identity.json), [guest provisioning/labels](evidence/t4/usb-gnome/ssh-provisioning.txt). |
| Grabbed shutdown and fixed resource policy | [GNOME shutdown timing](evidence/t4/usb-gnome/shutdown-grabbed.json), [Hyprland shutdown timing](evidence/t4/usb-hyprland-qt/shutdown-grabbed.json), [timestamped host capacity/configuration observations](evidence/t4/capacity.jsonl). |
| Development helper isolation | [Ordinary image filesystem audit](evidence/t4/production-helper-audit.json), [private composition audit](evidence/t4/development-helper-audit.json). |
| Review P1: ACLs, reconnects and low-integrity denial | [Pinned QEMU endpoints and raw DACL SDDL](evidence/t4/review-transport-acl/transport-ready.json), [real lowered-token connection results](evidence/t4/review-transport-acl/low-integrity.json). Both clients receive Windows error 10013; normal clients reconnect before and after. |
| Review P1: long artifact-state paths, concurrent clients and listener audit | [Long-path context](evidence/t4/review-transport-known-folder/context.json), [lowered-token checks](evidence/t4/review-transport-known-folder/low-integrity.json), [held input connection](evidence/t4/review-dual-qmp/result.json), [simultaneous native vmctl check](evidence/t4/review-dual-qmp/vmctl-self-check.json), [actual VM TCP listeners](evidence/t4/review-dual-qmp/listeners.json). Only pinned SSH listens on TCP. [Isolated runtime activation](evidence/t4/review-runtime-activation-final.stdout.log) also passes with private QMP. |
| Review P2: delayed GNOME login | [Full timestamped retry trace](evidence/t4/review-late-login/timeline.jsonl), [result](evidence/t4/review-late-login/result.json), [Cua window geometry](evidence/t4/review-late-login/cua-window-after-resize.json). GDM stayed stopped beyond 25 seconds after resize; session 1 → 50 recovered 1596×953 without another host resize. |
| Review P3: real PrintWindow motion and frozen-frame refusal | [Raw commands, timing, frame proofs and PNG hashes](evidence/t4/review-capture/result.json), [positive content-change sidecar](evidence/t4/review-capture/moving-printwindow.frame.json), [unchanged-frame negative](evidence/t4/review-capture/paused-printwindow-1.stderr.log). Changing GTK pixels passed; both paused-guest attempts failed with no output PNG. Presentation ticks are null for PrintWindow. |
| Review teardown probes | [Real caption close](evidence/t4/review-lifecycle/close-window/result.json), [Cua close action](evidence/t4/review-lifecycle/close-window/cua-action.json), [SendInput Ctrl-C](evidence/t4/review-lifecycle/ctrl-c-before-resize-recovery/result.json), [Cua Ctrl-C action](evidence/t4/review-lifecycle/ctrl-c-before-resize-recovery/cua-action.json). Both processes exited, keys/sockets were removed and stale attachment failed. The later console-isolation refinement and forced-exit/restart gate still need their final rerun. |

The original desktop observations below predate the base-stack rewrite and
identify their frozen installed disk/source explicitly. Post-review transport
results use the rebased Windows implementation. The first compositor-switch
[negative trace](evidence/t4/review-session-hyprland/timeline.jsonl) exposed SDL
adopting the guest's smaller bootstrap mode as the host target. The corrected
build preserves the acknowledged target while EDID is unchanged. Its cold-boot
session regression and final console-isolation/forced-exit probes remain
queued for host capacity and are not claimed complete here.

These are real VM observations from 17–18 November 2026, separate from the
feasibility spike. The host runs Windows 11 Pro build 26300 with a Radeon
RX 9070 XT and 150% display scaling. Desktop VMs use 8 GiB and four vCPUs;
the shared builder retains 16 GiB and eight vCPUs. Admission retains a 14 GiB
host reserve. Tests run serially when host capacity permits.

The pinned portable runtime is the closure in
[`runtime-lock.json`](../../crates/ilex-tooling-windows/assets/runtime-lock.json).
No Windows feature, system install, distro lifecycle or global setting change
was used. The development helpers are absent from the ordinary image and
present only in the private development composition. The latter passed the
real bootc lint (14 passed, one skipped).

## GNOME

The initial actual ilex installed disk came through the retained WSL path,
with its owner receipt and manifest copied to Windows and verified on import.
It booted natively with WHPX, SDL GL and virtio-vga-gl. Guest facts: Fedora 45,
kernel 7.3.4-300.fc45, GNOME/Mutter 51.1-1.fc45, Mesa 26.2.6-1.fc45,
SELinux enforcing.

| Check | Observation |
| --- | --- |
| Renderer | Independent GTK Wayland GLArea reports `virgl (AMD Radeon RX 9070 XT)`, OpenGL ES 3.2 Mesa 26.2.6. |
| Sustained motion | 5,028 callbacks in 70.07 s; first five seconds 30.2/s, then 74.8–75.2/s in five-second samples. These are application callbacks, not input-to-photon measurements. |
| Resize | Physical client 1280×800 → 1596×953; Mutter current/preferred mode and resize acknowledgement match. Native PNG dimensions match those client pixels. |
| Keyboard | Three repetitions of Super, Super+E, Alt+Tab and Ctrl+Esc through physical SendInput scan codes; QEMU remains foreground/grabbed. Guest evdev records balanced downs/ups and no held tested keys. Right-Ctrl+G escapes. |
| Focus release | While host Shift remains physically held, Cua focuses the owned cover window; guest receives Shift-up before host release. All recorded assertions pass. |
| Clipboard | Bidirectional multilingual Unicode and emoji pass; two successive host reads agree. Host/guest transfer counters remain unchanged over a 65 s idle interval. No clipboard payload is logged. |
| Re-login/lock | GDM invocation changes, adapter deactivates/reactivates automatically, Unicode transfers pass again. Lock disables transfers and unlock reactivates the channel. |
| Capture | Independent native vmctl self-check passes. Fresh WGC client crops before/after resize; two fully occluded frames have advancing timestamps and changing pixels. Minimize returns an unavailable/no-fresh-frame reason and produces no PNG; restore captures fresh 1596×953 content. |

Actual native evidence attached to the PR:
[before resize](https://github.com/user-attachments/assets/467239f4-479a-416d-8860-210a693416b5),
[after resize](https://github.com/user-attachments/assets/1171d0ce-cdb8-47e9-9572-8500944f665f),
[motion recording](https://github.com/user-attachments/assets/94ca1d1e-2bd4-4347-b5f6-d569c7cbed29).
The GIF samples 14 distinct fresh native frames over roughly ten seconds;
its sampling rate is separate from the application's callback rate.

## Builder delivery and repeat launches

The normal `cargo xtask vm --gui --persist` path submitted job
`7e28a556-cbe5-4efb-be20-e2457f0bcbe3` from signed source head
`885f2eafa1a128db9dad7f72daa89335ef4464e3`, snapshot
`sha256:b4dec9e92122576613cf1c723b9e30601a8bd9eee566e03e5f5f9f8f991c9c41`.
The job exited zero. Its verified 6,073,090,048-byte installed qcow2 has SHA256
`dfe2e5ed22b21d36320c6140687478eab046d6c7ff70d05f9563b1669799c2a6`.
The receipt identifies this checkout, job and source, and promotion keeps the
installed base immutable. No `ILEX_DISK` override was used for this command.

The default command booted that disk into GNOME and exited zero after guest
poweroff. It waited with the configured RAM/reserve intact, then started after
the supported idle-checked builder shutdown released its 16 GiB. A second
native cold boot used the same persistent overlay and firmware store. Both
boots reached GNOME with SDL's non-activating startup hint.

Both boots accepted their correct pins and rejected deliberately wrong pins.
The second run had different host-pin and access-key hashes and authenticated
successfully against the same persistent disk. The volatile authorized-key
file matched the firmware public credential, remained root-only (directory
0700, file 0600), and had the expected SELinux labels under enforcing mode.

The builder-delivered disk repeated the real host keyboard matrix and
bidirectional Unicode checks. Exactly 262,144 UTF-8 bytes of ASCII and emoji
transferred; payloads one emoji over the limit were refused in both directions,
and ordinary transfers recovered. Motion delivered 5,977 callbacks in 80.08 s,
with steady samples around 75/s. Host/guest counters stayed unchanged over
65 s. The second boot also passed automatic clipboard reactivation after a
new GDM invocation, multilingual transfer, and another 65 s no-loop interval.

Native vmctl self-check and the actual stdio MCP test passed on this disk.
Cua resized 1280×800 to 1596×953; Mutter mode matched, old screenshot
coordinates were rejected and a fresh screenshot restored input mapping.
A separate native recording sampled 12 fresh frames, with 11 distinct pixel
images; freshness comes from advancing WGC timestamps rather than pixel changes.

## Hyprland / Qt

The separate disposable Fedora 45 overlay used
`dev-session/checks/hyprland-research.sh`: Hyprland 0.57.2-1.fc45,
Qt base/declarative 6.11.2-3.fc45, Qt Wayland 6.11.2-1.fc45 and
wl-clipboard 2.3.0-2.fc45. This uses the guest's transient `bootc usr-overlay`;
it adds neither Hyprland nor development helpers to published images.
The fixture selects the Wayland session through AccountsService and uses
Hyprland's current Lua configuration.

| Check | Observation |
| --- | --- |
| Qt renderer | Native Wayland Qt Quick, OpenGL RHI; `virgl (AMD Radeon RX 9070 XT)`, OpenGL 4.3 Mesa 26.2.6. |
| Sustained motion | 6,750 frames / 90.315 s; interval mean 13.368 ms, median 13 ms, p95 15 ms, p99 16 ms. After re-login: 6,751 / 90.624 s, mean 13.335 ms, same median/p95/p99. Approximately 75 application frames/s. |
| Resize | Actual monitor and client 1596×953 → 1496×903, automatically acknowledged. Aquamarine's old mode list required the kernel's freshly advertised preferred EDID timing. The helper validates that exact connector, dimensions, checksum and progressive timing, then applies it through runtime Lua without writing configuration. |
| Keyboard | Real SendInput matrix passes, including three observed Super+E compositor actions and Right-Ctrl+G escape; all tested guest keys released. Focus-loss probe observes guest Shift-up while host Shift is still held. |
| Qt clipboard | Physical Ctrl+A/C and Tab/Ctrl+V into Qt Quick TextInputs transfer multilingual Unicode in both directions. The receiver is masked. Counters stay unchanged for 65 s. A real GDM restart changes invocation, automatically reconnects the adapter, and repeats the Qt clipboard/no-loop checks. |
| Capture | Real MCP resize/stale-coordinate check passes. Fully occluded 1496×903 WGC frames have advancing timestamps and changing Qt pixels. Independently verified minimized HWND yields an explicit no-fresh-frame error and no PNG. Restore yields a fresh crop. |

## Shutdown regression and final pointer profile

A grabbed guest shutdown exposed a pinned QEMU SDL title-update deadlock.
The final profile uses `qemu-xhci` with `usb-tablet`, retaining the virtio
keyboard. Unlike virtio-input's activation changes, USB HID keeps its absolute
handler active across driver shutdown; see the pinned QEMU
[virtio input](https://github.com/qemu/qemu/blob/v11.1.3/hw/input/virtio-input-hid.c)
and [HID implementation](https://github.com/qemu/qemu/blob/v11.1.3/hw/input/hid.c).
No binary patch, injected shim or system setting change is involved.

The revised profile cold-booted the same persistent GNOME overlay with new
SSH credentials. It repeated clipboard byte limits, the 65 s no-loop check,
real keyboard matrix and MCP resize/capture checks. GTK delivered 5,957
callbacks / 80.106 s, with steady five-second samples 73.4–75.2/s.
`shutdown-grabbed` then kept SDL grabbed, requested guest poweroff over pinned
SSH, and observed the actual QEMU process exit naturally in 3.406 s.
The revised Hyprland profile then cold-booted the separate persistent research
disk and recreated its transient package layer with the checked-in fixture.
Native preparation deployed the exact checked-in adapter (SHA256
`44e0496bcf0a114a2bb5448b298453b9171c03f4ca0864a494b84c457a8bdc1b`),
with no manual helper refresh. Qt repeated Unicode transfer, a 65 s no-loop
interval, and automatic 1280×800 → 1596×953 resize with fresh native PNGs/GIF.
It delivered 6,750 frames / 90.273 s; mean interval 13.346 ms, median 13 ms,
p95/p99 16 ms. The keyboard matrix repeated all three Super+E actions.
Grabbed shutdown exited the actual QEMU process naturally in 3.469 s.

An independent human-input QMP connection stayed open while native vmctl
completed its real self-check through the control endpoint, including two
fresh WGC captures; the human connection remained usable afterward.
Both desktop guests and their launchers exited. Their writable disks stay in
this checkout's state for review; run credentials are removed. The shared
builder's configuration remains 16 GiB/eight vCPUs and its supported shutdown
retains its identity and disks. No other checkout/VM cleanup or prune is used.

## Reproduction

Use `windows-e2e.py --info <owned vm.json> --artifacts <dir> --vmctl <native exe>`
with `provision`, `ssh-identity`, `clipboard --limits`, `relogin`, `lock`,
`input`, `focus`, `qt-clipboard`, `qt-relogin`, or `shutdown-grabbed`.
Cua selects the exact owned HWND before
input and performs window geometry/focus/minimize actions. Acceptance keyboard
events use real host SendInput rather than posted window messages.
`windows-mcp.py --resize` exercises the real stdio MCP server, rejects old
screenshot coordinates after a Cua resize and recovers with a fresh screenshot.
No unit tests were added or run.
