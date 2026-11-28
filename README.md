# ilex

A personal Fedora bootc image with a GNOME or KDE Plasma desktop, built from one
`Containerfile` on `fedora-bootc:45` and tested end to end in QEMU.

The name is Latin for holly, an evergreen that stays green through winter.

## Install

Two live installers, one per edition, are released together on the
[releases page](https://github.com/Reidond/ilex/releases/latest). Write the ISO
you want to a USB stick and boot it:

| Edition | Desktop | Download |
| --- | --- | --- |
| ilex Glabra | GNOME | [ilex-glabra-installer.iso](https://github.com/Reidond/ilex/releases/latest/download/ilex-glabra-installer.iso) ([sha256](https://github.com/Reidond/ilex/releases/latest/download/ilex-glabra-installer.iso.sha256)) |
| ilex Kingiana | KDE Plasma | [ilex-kingiana-installer.iso](https://github.com/Reidond/ilex/releases/latest/download/ilex-kingiana-installer.iso) ([sha256](https://github.com/Reidond/ilex/releases/latest/download/ilex-kingiana-installer.iso.sha256)) |

`ilex-installer.iso` (the name the previous installer used) is still published
and is ilex Glabra's file, byte for byte.

## Develop

```sh
cargo ilexctl build
cargo ilexctl test ephemeral::
```

See [CLAUDE.md](CLAUDE.md) for the development workflow.
