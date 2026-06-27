# PICKLE OS — build & run automation
#
# Requirements (see README.md for full setup):
#   - Rust nightly toolchain (pinned via rust-toolchain.toml)
#   - rust-src + llvm-tools-preview components
#   - bootloader_linker (`cargo install bootloader_linker --version 0.1.7`)
#   - qemu-system-x86_64
#
# Common targets:
#   make build         # compile the kernel (release)
#   make image         # produce the bootable disk image (bios.img)
#   make run           # boot in QEMU with serial on stdio (headless)
#   make run-display   # boot in QEMU with a graphical framebuffer window
#   make clean         # remove build artifacts
#
# NOTE: PICKLE OS now boots via the `bootloader` 0.11 crate (pixel framebuffer
# GUI) instead of the old `bootimage`/`bootloader` 0.9 VGA-text flow. The kernel
# is built in *release* mode (a debug kernel is ~12 MiB and far too slow to boot
# under TCG emulation); `bootloader_linker` then wraps the kernel ELF together
# with the BIOS bootloader stages into a single bootable `bios.img`.

TARGET        := x86_64-pickleos
KERNEL_ELF     = target/$(TARGET)/release/kernel
BIOS_IMG       = target/$(TARGET)/release/bios.img
LINKER         = bootloader_linker
QEMU           = qemu-system-x86_64

# Two scratch SATA data disks attached to an ICH9 AHCI controller. The kernel's
# AHCI driver enumerates these as sata0 / sata1; NextFS lives on sata1 and the
# user-space `init` launches programs from it.
NEXTFS_DISK0   = disk0.img
NEXTFS_DISK1   = disk1.img
DISK_SIZE_MB   = 16
AHCI_ARGS      = -device ich9-ahci,id=ahci \
		 -drive id=d0,format=raw,file=$(NEXTFS_DISK0),if=none \
		 -drive id=d1,format=raw,file=$(NEXTFS_DISK1),if=none \
		 -device ide-hd,drive=d0,bus=ahci.0 \
		 -device ide-hd,drive=d1,bus=ahci.1
NET_ARGS       = -device e1000,netdev=net0 \
		 -netdev user,id=net0,hostfwd=tcp::5555-:7
QEMU_COMMON    = -drive format=raw,file=$(BIOS_IMG) -m 256M -no-reboot $(AHCI_ARGS) $(NET_ARGS)

# Minimum framebuffer geometry requested from the bootloader (the kernel's
# BOOTLOADER_CONFIG asks for at least 1024x768; QEMU's std VGA gives 1280x720).
FB_WIDTH       := 1024
FB_HEIGHT      := 768

.PHONY: all build image run run-display debug test clean fmt clippy userspace disks

all: image

userspace:
	$(MAKE) -C userspace

build: userspace
	cargo build --release

# Wrap the release kernel ELF + BIOS bootloader stages into a bootable image.
image: build
	$(LINKER) build $(KERNEL_ELF) -W $(FB_WIDTH) -H $(FB_HEIGHT) \
	        -o target/$(TARGET)/release

# Create the scratch SATA data disks if they don't exist.
disks:
	@test -f $(NEXTFS_DISK0) || (echo ">> creating $(NEXTFS_DISK0) ($(DISK_SIZE_MB) MiB)"; qemu-img create -f raw $(NEXTFS_DISK0) $(DISK_SIZE_MB)M >/dev/null)
	@test -f $(NEXTFS_DISK1) || (echo ">> creating $(NEXTFS_DISK1) ($(DISK_SIZE_MB) MiB)"; qemu-img create -f raw $(NEXTFS_DISK1) $(DISK_SIZE_MB)M >/dev/null)

# Headless: kernel serial console is bridged to your terminal (stdio).
# You will see the boot log, scheduler, IPC and shell prompt here.
run: image disks
	$(QEMU) $(QEMU_COMMON) -serial stdio -display none

# Graphical: opens a QEMU window showing the pixel framebuffer desktop.
# Keyboard input goes to the in-kernel shell (type `help`).
run-display: image disks
	$(QEMU) $(QEMU_COMMON) -serial stdio

# Boot and wait for a GDB stub on :1234 (connect with `target remote :1234`).
debug: image disks
	$(QEMU) $(QEMU_COMMON) -serial stdio -display none -s -S

# Run the kernel's integration tests (isa-debug-exit based).
test:
	cargo test

fmt:
	cargo fmt

clippy:
	cargo clippy

clean:
	cargo clean
	$(MAKE) -C userspace clean
	rm -f *.log
