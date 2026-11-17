# Target setup handoff, version 1

`ilex-installer-choice apply` reserves `/run/ilex-installer/setup.json` and
writes the same nonsecret record to the target's `/etc/ilex/setup.json` through
the generated kickstart's `%post`. `schema: 1` identifies this contract.

- `composition`: the table's selected machine and ordered shells tag.
- `image`: the validated install/update reference; a local testing build can
  map every composition to one signed tag without changing `composition`.
- `profile`: `id` and profile `schema`, both null until profile support exists.
- `security.capabilities`: an empty map, reserved for observed capabilities.
- `security.tpm`: unencrypted installs retain requested mode `unspecified`,
  capability `unknown` and status `not-configured`. Encrypted completion writes
  requested mode `auto`, observed capability/reason, and `enrolled-unverified`
  or `passphrase-only`. Enrollment stays pending until PR19 verifies a boot.

The local encrypted-install completion helper updates this record without
changing its composition, image or profile. Enrollment details live in the
encrypted target's `/var/lib/ilex/security/enrollment.json`; see
`docs/installer-enrollment.md`. This record performs no user provisioning. Readers
must decline unsupported schema versions, tolerate additive fields and inspect
actual system state before reporting success. Never put passwords, recovery
material, tokens or private keys in this record or the installer catalogue.
The catalogue's empty `profiles` and `security-capabilities` fields reserve
future nonsecret discovery; selecting a profile must not change the OCI tag.

Keep future installer enrollment/completion helpers below `installer/` so
their bytes enter the installer fingerprint automatically. If a helper is
shared from another source directory, add that directory to
`INSTALLER_INPUTS` in `crates/xtask/src/iso.rs` before using it.

The pre-Anaconda chooser's separate `/run/ilex-installer/choice` record is
schema 1 with `composition` only. It is written atomically and validated
against the generated catalogue again by `apply`. Kickstarts may still write
a plain composition tag. Future profile/security fields require explicit
validation in both producer and consumer; the chooser handles no passwords,
passphrases or recovery material. None belong in this choice record, URLs or
the generated catalogue.
