//! Bake the target triple into the binary.
//!
//! The release artifact is named after the platform it was built for, and
//! `magictree update` downloads the one that matches itself, so the triple has
//! to be the one this build was compiled for rather than whatever the machine
//! running it happens to be.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=MAGICTREE_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
