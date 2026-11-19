set -eu

# Work only with three sparse files created inside the disposable guest.
scratch=$(mktemp -d /var/tmp/ilex-layout.XXXXXXXX)
mountpoint="$scratch/root"
mapper="ilex-layout-crypt-$$"
linear="ilex-layout-linear-$$"
encrypted=
plaintext=
disposable=
cleanup() {
    if mountpoint -q "$mountpoint"; then umount "$mountpoint"; fi
    if [ -b "/dev/mapper/$linear" ]; then dmsetup remove "$linear"; fi
    if [ -b "/dev/mapper/$mapper" ]; then cryptsetup close "$mapper"; fi
    for device in "$encrypted" "$plaintext" "$disposable"; do
        if [ -n "$device" ]; then losetup --detach "$device"; fi
    done
    rm -f -- "$scratch/encrypted" "$scratch/plaintext" "$scratch/disposable"
    rmdir -- "$mountpoint" "$scratch"
}
trap cleanup EXIT
mkdir "$mountpoint"
truncate -s 256M "$scratch/encrypted" "$scratch/plaintext" "$scratch/disposable"
encrypted=$(losetup --find --show "$scratch/encrypted")
plaintext=$(losetup --find --show "$scratch/plaintext")
disposable=$(losetup --find --show "$scratch/disposable")

printf %s ilex-disposable-layout-check |
    cryptsetup luksFormat --type luks2 --batch-mode --pbkdf pbkdf2 +        --pbkdf-force-iterations 1000 --key-file - "$encrypted"
printf %s ilex-disposable-layout-check |
    cryptsetup open --key-file - "$encrypted" "$mapper"
udevadm settle
crypt="/dev/mapper/$mapper"

reject_layout() {
    if message=$(ilex-enroll layout --root "$mountpoint" 2>&1); then
        echo 'Enrollment accepted a partly encrypted filesystem' >&2
        exit 1
    fi
    if ! printf '%s\n' "$message" | grep -Fq 'backing device is not a direct dm-crypt LUKS2 mapping'; then
        printf 'Unexpected layout error: %s\n' "$message" >&2
        exit 1
    fi
}

# A single Btrfs member where an upper linear map joins two backing paths.
# This is the review's counterexample to collecting only the set of LUKS UUIDs.
encrypted_sectors=$(blockdev --getsz "$crypt")
plaintext_sectors=$(blockdev --getsz "$plaintext")
printf '0 %s linear %s 0\n%s %s linear %s 0\n' +    "$encrypted_sectors" "$crypt" "$encrypted_sectors" "$plaintext_sectors" "$plaintext" |
    dmsetup create "$linear"
mkfs.btrfs -q -f "/dev/mapper/$linear"
mount "/dev/mapper/$linear" "$mountpoint"
reject_layout
umount "$mountpoint"
dmsetup remove "$linear"
echo 'Rejected single-member Btrfs over mixed dm-linear branches'

# Multiple Btrfs members, one directly encrypted and one plaintext.
mkfs.btrfs -q -f -d single -m single "$crypt" "$disposable"
mount "$crypt" "$mountpoint"
reject_layout
echo 'Rejected multi-member Btrfs with an encrypted member and a plaintext member'
