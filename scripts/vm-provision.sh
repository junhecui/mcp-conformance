#!/usr/bin/env bash
# F-08 / ADR-010 -- build the pinned Linux VM's artifacts. Idempotent: safe to re-run.
#
# Produces, under $MCP_VM_DIR (default ~/vm/f-08), deliberately OUTSIDE the repo:
#   image/ubuntu-24.04-server-cloudimg-amd64.img   the pinned, signature-verified base
#   image/SHA256SUMS, SHA256SUMS.gpg               the signed sums it was verified against
#   seed.iso                                       NoCloud cloud-init seed
#   disk.qcow2                                     per-VM overlay on the pinned base
#   ssh/id_ed25519{,.pub}                          VM-only keypair
#
# Runs inside the WSL2 Ubuntu-24.04 distro (see ADR-010's Windows provisioning record for
# why the hypervisor lives there rather than on Windows). Needs: qemu-utils, xorriso,
# qemu-system-x86, and /dev/kvm readable by the invoking user.
set -euo pipefail

# ---- The pin. Changing any of these is a deliberate act; see ADR-010. -------------------
SERIAL="20260725"   # exact dated archive serial -- NEVER the moving `release/` symlink
ARCH="amd64"
# The signing key, pinned by full fingerprint. `--keyring ubuntu-cloudimage-keyring.gpg`
# alone would accept a signature from EITHER key that keyring holds -- 1A5D6C4C7DB87C81
# (UEC Image Automatic Signing Key) or 7FF3F408476CF100 (Ubuntu Cloud Image Builder) -- so
# without this, the re-runnable check would only assert "signed by some key in Canonical's
# keyring" while ADR-010 and tasks.md both assert the stronger "signed by this key, not the
# similarly-named other one". This makes the script enforce what the records claim.
GPG_FPR="D2EB44626FDDC30B513D5BB71A5D6C4C7DB87C81"
# ----------------------------------------------------------------------------------------

BASE_URL="https://cloud-images.ubuntu.com/releases/noble/release-${SERIAL}"
IMG="ubuntu-24.04-server-cloudimg-${ARCH}.img"
KEYRING="/usr/share/keyrings/ubuntu-cloudimage-keyring.gpg"
DISK_SIZE="${MCP_VM_DISK_SIZE:-40G}"

VM_DIR="${MCP_VM_DIR:-$HOME/vm/f-08}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# MCP_VM_DISK_SIZE is handed to qemu-img, where a value carrying extra option syntax would be
# parsed rather than rejected. Same reasoning as the MCP_VM_* numeric checks in
# scripts/vm-lib.sh; a digits-plus-single-unit-suffix check is total.
[[ "$DISK_SIZE" =~ ^[0-9]{1,6}[KMGT]?$ ]] \
  || die "MCP_VM_DISK_SIZE must be digits with an optional K/M/G/T suffix, got: ${DISK_SIZE@Q}"

# python3 renders the cloud-init template below. It was missing from this list, so a host
# without it failed *after* the fetch-and-verify work instead of in the preflight.
for t in qemu-img qemu-system-x86_64 xorriso ssh-keygen gpgv curl sha256sum python3; do
  command -v "$t" >/dev/null || die "missing \`$t\`. apt-get install qemu-utils qemu-system-x86 xorriso python3"
done
[[ -r /dev/kvm ]] || die "/dev/kvm not readable. Run: sudo adduser \"$USER\" kvm, then open a new shell."
[[ -r "$KEYRING" ]] || die "missing $KEYRING. apt-get install ubuntu-cloudimage-keyring"

mkdir -p "$VM_DIR/image" "$VM_DIR/ssh"

# -- 1. signed sums, verified against Canonical's UEC key -------------------------------
# The keyring comes from the distro package, i.e. already authenticated by Ubuntu's archive
# key, rather than fetched over TLS from a keyserver. That is a second, independent trust
# root; see ADR-010's provisioning record for what this does and does not establish.
say "fetching and verifying SHA256SUMS for serial ${SERIAL}"
curl -fsSL -o "$VM_DIR/image/SHA256SUMS"     "$BASE_URL/SHA256SUMS"
curl -fsSL -o "$VM_DIR/image/SHA256SUMS.gpg" "$BASE_URL/SHA256SUMS.gpg"
GPGV_STATUS="$VM_DIR/image/SHA256SUMS.gpgv-status"
gpgv --status-fd 3 --keyring "$KEYRING" \
     "$VM_DIR/image/SHA256SUMS.gpg" "$VM_DIR/image/SHA256SUMS" 3>"$GPGV_STATUS" \
  || die "GPG verification of SHA256SUMS FAILED -- do not use this image"
# gpgv's machine-readable VALIDSIG line carries the full fingerprint; the human output does
# not distinguish the two keyring keys reliably enough to grep for.
grep -qE "^\[GNUPG:\] VALIDSIG ${GPG_FPR}( |\$)" "$GPGV_STATUS" \
  || die "SHA256SUMS is signed, but NOT by the pinned key ${GPG_FPR}.
       gpgv status is in $GPGV_STATUS. Refusing to continue."
say "signature is from the pinned key ${GPG_FPR}"

# -- 2. the image itself, checked against the signed sums (always, not only on download) --
# Downloaded to a temp path and renamed only once curl succeeded, for the same reason
# `store`'s blob store writes temp-then-rename (crates/store/src/lib.rs): otherwise an
# interrupted download leaves a partial file at the final path, and the next run wedges on a
# checksum failure with no hint that deleting it is the fix.
if [[ ! -f "$VM_DIR/image/$IMG" ]]; then
  say "downloading $IMG"
  IMG_TMP="$VM_DIR/image/.$IMG.partial.$$"
  curl -fSL --progress-bar -o "$IMG_TMP" "$BASE_URL/$IMG" \
    || { rm -f "$IMG_TMP"; die "download of $IMG failed; nothing was left at the final path"; }
  mv -f "$IMG_TMP" "$VM_DIR/image/$IMG"
fi
say "verifying $IMG against the signed sums"
( cd "$VM_DIR/image" && grep " \*${IMG}\$" SHA256SUMS | sha256sum -c - ) \
  || die "image sha256 does not match the signed SHA256SUMS -- refusing to continue"

# -- 3. VM-only keypair -----------------------------------------------------------------
if [[ ! -f "$VM_DIR/ssh/id_ed25519" ]]; then
  say "generating a VM-only ed25519 keypair"
  ssh-keygen -t ed25519 -N '' -C "mcp-conformance-f08-vm" -f "$VM_DIR/ssh/id_ed25519" >/dev/null
fi
chmod 700 "$VM_DIR/ssh"; chmod 600 "$VM_DIR/ssh/id_ed25519"

# -- 4. NoCloud seed ISO ----------------------------------------------------------------
# The rendered user-data is written into the VM dir, never the repo: it embeds this host's
# public key, which is per-host state. .gitignore names scripts/vm/user-data as well, so the
# template's promise that the rendered copy is never committed is structural rather than only
# a comment.
say "rendering user-data and building seed.iso"
SEED_SRC="$VM_DIR/seed-src"; rm -rf "$SEED_SRC"; mkdir -p "$SEED_SRC"
PUBKEY="$(cat "$VM_DIR/ssh/id_ed25519.pub")"
PUBKEY="$PUBKEY" python3 -I -c '
import os, sys
tpl = open(sys.argv[1], encoding="utf-8").read()
assert "@SSH_PUBKEY@" in tpl, "template lost its @SSH_PUBKEY@ placeholder"
open(sys.argv[2], "w", encoding="utf-8", newline="\n").write(
    tpl.replace("@SSH_PUBKEY@", os.environ["PUBKEY"].strip()))
' "$REPO_ROOT/scripts/vm/user-data.template" "$SEED_SRC/user-data"
cp "$REPO_ROOT/scripts/vm/meta-data" "$SEED_SRC/meta-data"
# -volid CIDATA is load-bearing: the NoCloud datasource finds its seed by volume label.
xorriso -as mkisofs -quiet -output "$VM_DIR/seed.iso" -volid CIDATA -joliet -rock \
  "$SEED_SRC/user-data" "$SEED_SRC/meta-data"

# -- 5. overlay disk --------------------------------------------------------------------
# A qcow2 overlay keeps the pinned base image byte-identical and read-only for the VM's
# whole life, so the pin cannot be mutated by anything that happens inside the guest.
# Note what is NOT verified: this script verifies the base image on every run, but never
# disk.qcow2 itself -- see precondition 5 on F-08 in docs/tasks.md.
if [[ ! -f "$VM_DIR/disk.qcow2" ]]; then
  say "creating disk.qcow2 ($DISK_SIZE) backed by the pinned image"
  qemu-img create -q -f qcow2 -F qcow2 -b "$VM_DIR/image/$IMG" "$VM_DIR/disk.qcow2" "$DISK_SIZE"
fi

say "done. artifacts in $VM_DIR"
ls -l "$VM_DIR"
