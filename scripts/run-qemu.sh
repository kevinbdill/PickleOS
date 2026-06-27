#!/usr/bin/env bash
# Build and boot PICKLE OS in QEMU.
#
# Usage:
#   scripts/run-qemu.sh            # headless, serial -> your terminal
#   scripts/run-qemu.sh --display  # open the graphical framebuffer window
#
# PICKLE OS boots via the `bootloader` 0.11 crate (pixel framebuffer GUI). The
# kernel is built in release mode (a debug kernel is far too slow under TCG) and
# `bootloader_linker` wraps it into a bootable BIOS image (bios.img).
set -euo pipefail

cd "$(dirname "$0")/.."

# Make sure the Rust toolchain is on PATH (rustup default location).
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

KERNEL_ELF="target/x86_64-pickleos/release/kernel"
BIOS_IMG="target/x86_64-pickleos/release/bios.img"
OUT_DIR="target/x86_64-pickleos/release"

echo ">> Building release kernel..."
cargo build --release

echo ">> Linking bootable image with bootloader_linker..."
bootloader_linker build "$KERNEL_ELF" -W 1024 -H 768 -o "$OUT_DIR"

# Two scratch SATA data disks for the AHCI driver / NextFS.
DISK0="disk0.img"; DISK1="disk1.img"
for d in "$DISK0" "$DISK1"; do
  [ -f "$d" ] || qemu-img create -f raw "$d" 16M >/dev/null 2>&1
done

QEMU_ARGS=(
  -drive "format=raw,file=${BIOS_IMG}" -m 256M -no-reboot
  -device ich9-ahci,id=ahci
  -drive id=d0,format=raw,file="$DISK0",if=none
  -drive id=d1,format=raw,file="$DISK1",if=none
  -device ide-hd,drive=d0,bus=ahci.0
  -device ide-hd,drive=d1,bus=ahci.1
  -serial stdio
)

if [ "${1:-}" != "--display" ]; then
  QEMU_ARGS+=(-display none)
fi

echo ">> Booting PICKLE OS in QEMU..."
exec qemu-system-x86_64 "${QEMU_ARGS[@]}"
