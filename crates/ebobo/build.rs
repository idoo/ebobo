//! macOS: statically linked SDL2 contains Objective-C `@available(...)` checks, which call
//! `___isPlatformVersionAtLeast` from clang's compiler-rt. rustc does not link compiler-rt's
//! osx runtime, so link it explicitly from the active toolchain.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let output = Command::new("xcrun")
        .args(["clang", "--print-resource-dir"])
        .output()
        .expect("xcrun clang --print-resource-dir (Xcode command line tools required)");
    let resource_dir = String::from_utf8(output.stdout).expect("utf-8 path");
    let lib_dir = format!("{}/lib/darwin", resource_dir.trim());
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}
