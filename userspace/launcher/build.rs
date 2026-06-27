// Link user programs at 0x400000 to match the kernel's user-space layout.
// The default lld base (0x200000) collides with the kernel's lower-half
// mapping, causing the ELF loader to fail with PageAlreadyMapped.
fn main() {
    println!("cargo:rustc-link-arg=--image-base=0x400000");
}
