#!/usr/bin/env bash
# Boot PICKLE OS in QEMU and halt for a GDB connection.
#
# In one terminal:
#   scripts/debug-qemu.sh
# In another:
#   gdb target/x86_64-pickleos/debug/kernel
#   (gdb) target remote :1234
#   (gdb) continue
set -euo pipefail

cd "$(dirname "$0")/.."

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

KERNEL_ELF="target/x86_64-pickleos/release/kernel"
BIOS_IMG="target/x86_64-pickleos/release/bios.img"
OUT_DIR="target/x86_64-pickleos/release"

echo ">> Building bootable image..."
cargo build --release
bootloader_linker build "$KERNEL_ELF" -W 1024 -H 768 -o "$OUT_DIR"

echo ">> Booting PICKLE OS (paused, GDB stub on :1234)..."
exec qemu-system-x86_64 \
  -drive "format=raw,file=${BIOS_IMG}" \
  -m 256M -no-reboot \
  -serial stdio -display none \
  -s -S
