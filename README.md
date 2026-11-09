# ilex

A personal Fedora bootc image with a GNOME desktop, built from one
`Containerfile` on `fedora-bootc:45` and tested end to end in QEMU.

The name is Latin for holly, an evergreen that stays green through winter.

```sh
cargo xtask build
cargo xtask test ephemeral::
```

See [CLAUDE.md](CLAUDE.md) for the development workflow.
