#!/usr/bin/env bash
# Boot PICKLE OS headless, feed optional shell input, capture serial for N seconds.
#
# Usage: scripts/test-boot.sh [seconds] [input]
set -uo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
SECS="${1:-25}"
INPUT="${2:-}"

KERNEL_ELF="target/x86_64-pickleos/release/kernel"
BIOS_IMG="target/x86_64-pickleos/release/bios.img"
OUT_DIR="target/x86_64-pickleos/release"

cargo build --release >/dev/null 2>&1
bootloader_linker build "$KERNEL_ELF" -W 1024 -H 768 -o "$OUT_DIR" >/dev/null 2>&1

# Two scratch SATA data disks for the AHCI driver / NextFS (matches the Makefile).
DISK0="disk0.img"; DISK1="disk1.img"
for d in "$DISK0" "$DISK1"; do
  [ -f "$d" ] || qemu-img create -f raw "$d" 16M >/dev/null 2>&1
done
AHCI_ARGS=(-device ich9-ahci,id=ahci \
  -drive id=d0,format=raw,file="$DISK0",if=none \
  -drive id=d1,format=raw,file="$DISK1",if=none \
  -device ide-hd,drive=d0,bus=ahci.0 \
  -device ide-hd,drive=d1,bus=ahci.1)

OUT=$(mktemp)
if [ -n "$INPUT" ]; then
  printf '%b' "$INPUT" | timeout "$SECS" qemu-system-x86_64 -drive format=raw,file="$BIOS_IMG" -m 256M -no-reboot "${AHCI_ARGS[@]}" -serial stdio -display none > "$OUT" 2>&1
else
  timeout "$SECS" qemu-system-x86_64 -drive format=raw,file="$BIOS_IMG" -m 256M -no-reboot "${AHCI_ARGS[@]}" -serial stdio -display none > "$OUT" 2>&1
fi
cat "$OUT"
rm -f "$OUT"
pkill -9 qemu-system-x86 2>/dev/null
exit 0
