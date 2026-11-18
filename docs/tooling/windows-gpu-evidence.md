# T4 native Windows acceptance

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

Native boot and repeat-launch credential acceptance are still pending host
capacity. The old per-launch root credential bug is fixed by refreshing the
volatile firmware access-key file on every native boot. The checks require
the same persistent overlay to accept different launch keys and host pins;
a deliberately wrong pin must still be rejected.

## Hyprland / Qt

Still pending. The disposable research overlay uses
`dev-session/checks/hyprland-research.sh`; it does not add Hyprland to ilex or
published images. Its dependency resolution was exercised separately against
the Fedora 45 repositories, but that is not desktop acceptance.

## Reproduction

Use `windows-e2e.py --info <owned vm.json> --artifacts <dir> --vmctl <native exe>`
with `provision`, `ssh-identity`, `clipboard --limits`, `relogin`, `lock`,
`input`, `focus`, or `qt-clipboard`. Cua selects the exact owned HWND before
input and performs window geometry/focus/minimize actions. Acceptance keyboard
events use real host SendInput rather than posted window messages.
`windows-mcp.py --resize` exercises the real stdio MCP server, rejects old
screenshot coordinates after a Cua resize and recovers with a fresh screenshot.
No unit tests were added or run.
