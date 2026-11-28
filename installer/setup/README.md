# Target setup handoff, version 1

`ilex-installer-choice apply` reserves `/run/ilex-installer/setup.json` and
writes the same nonsecret record to the target's `/etc/ilex/setup.json` through
the generated kickstart's `%post`. `schema: 1` identifies this contract.

- `composition`: the table's selected machine and ordered shells tag.
- `image`: the validated install/update reference; a local testing build can
  map every composition to one signed tag without changing `composition`.
- `profile`: `id` and profile `schema` for a named public profile. For the
  custom daily user both are null and `mode` is `custom`. Legacy selections
  with both fields null and no mode request no automatic setup.
- `security.capabilities`: an empty map, reserved for observed capabilities.
- `security.tpm`: unencrypted installs retain requested mode `unspecified`,
  capability `unknown` and status `not-configured`. Encrypted completion writes
  requested mode `auto`, observed capability/reason, and `enrolled-unverified`
  or `passphrase-only`. Enrollment stays pending until PR19 verifies a boot.
  Unsupported storage records reason `unsupported-storage-layout`. The status
  is `passphrase-only` when the encrypted device can be independently verified,
  and `unavailable` when there is no verified device for a recovery key.

The local encrypted-install completion helper updates this record without
changing its composition, image or profile. Enrollment details live in the
encrypted target's `/var/lib/ilex/security/enrollment.json`; see
`docs/installer-enrollment.md`. This record performs no user provisioning. Readers
must decline unsupported schema versions, tolerate additive fields and inspect
actual system state before reporting success. Never put passwords, recovery
material, tokens or private keys in this record or the installer catalogue.
The catalogue's `profiles` map contains public IDs, their schema and display
names. Selecting a profile does not change the OCI tag.

The next installer's helper (`ilex-installerd`, the interface's **Daily user**
page or an answers file's `[daily]`) writes the same record in its configure
stage from the plan's daily-user choice, with exactly these fields: a public
profile is `{"id", "schema": 1}`, a custom user is `{"id": null, "schema": null,
"mode": "custom"}` and none is `{"id": null, "schema": null}`. A custom user's
name and capacity are not in the record; `/etc/issue.d/90-ilex-home-setup.issue`
names `sudo ilexctl home setup --name NAME [--disk-size GiB]` for it. A daily
user needs an encrypted root, and its name may not be the maintenance
administrator's.

Keep future installer enrollment/completion helpers below `installer/` so
their bytes enter the installer fingerprint automatically. If a helper is
shared from another source directory, add that directory to
`INSTALLER_INPUTS` in `crates/xtask/src/iso.rs` before using it.

The chooser's separate `/run/ilex-installer/choice` record is schema 1 with
`composition` and an optional `profile` field. An explicit null profile
requests custom setup. No daily profile omits the field and keeps Anaconda's
standard account and storage options. The chooser writes it atomically and `apply` validates
it against the generated catalogue again. Kickstarts may still write a plain
composition tag or an old two-field record. The chooser handles no passwords,
passphrases or recovery material.

A profile install requires an unlocked local maintenance administrator with
a usable password. Prehashed kickstart passwords use canonical SHA-512 crypt
or yescrypt with standard libxcrypt cost settings.

`ilexctl home setup` checks named profile metadata against the installed
image's catalogue and payload hash. It records a nonsecret intent in
`/var/lib/ilex/home-setup/state.json` before invoking the existing homectl
creation path. Capacity is checked before saving the intent. A custom intent
can be edited while it has no account, backing image or home directory.
A local or unrelated homed account with the requested name is refused. To
resume a created home, setup checks the allocated LUKS UUID and asks homed
to unlock and verify it with its native password prompt. The profile service
matches the recorded UID and UUID on
authenticated activation and applies the profile as the daily user.
