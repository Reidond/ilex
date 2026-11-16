# Target setup handoff, version 1

`ilex-installer-choice apply` reserves `/run/ilex-installer/setup.json` and
writes the same nonsecret record to the target's `/etc/ilex/setup.json` through
the generated kickstart's `%post`. `schema: 1` identifies this contract.

- `composition`: the table's selected machine and ordered shells tag.
- `image`: the validated install/update reference; a local testing build can
  map every composition to one signed tag without changing `composition`.
- `profile`: `id` and profile `schema`, both null until profile support exists.
- `security.capabilities`: an empty map, reserved for observed capabilities.
- `security.tpm`: requested mode `unspecified`, actual capability `unknown`,
  status `not-configured`. These are unmeasured/unconfigured states.

This record performs no security setup, capability probing, TPM enrollment,
encryption or user provisioning. No service consumes it yet. Future readers
must decline unsupported schema versions, tolerate additive fields and inspect
actual system state before reporting success. Never put passwords, recovery
material, tokens or private keys in this record or the installer catalogue.
The catalogue's empty `profiles` and `security-capabilities` fields reserve
future nonsecret discovery; selecting a profile must not change the OCI tag.

Keep future installer enrollment/completion helpers below `installer/` so
their bytes enter the installer fingerprint automatically. If a helper is
shared from another source directory, add that directory to
`INSTALLER_INPUTS` in `crates/xtask/src/iso.rs` before using it.
