//! macOS: statically linked SDL2 contains Objective-C `@available(...)` checks, which call
//! `___isPlatformVersionAtLeast` from clang's compiler-rt. rustc does not link compiler-rt's
//! osx runtime, so link it explicitly from the active toolchain.
//!
//! Windows (mingw-w64): compiles `packaging/windows/ebobo.rc` so that ebobo.exe has its icon.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (os.as_str(), env.as_str()) {
        ("macos", _) => link_compiler_rt(),
        ("windows", "gnu") => link_icon(),
        _ => {}
    }
}

fn link_compiler_rt() {
    let output = Command::new("xcrun")
        .args(["clang", "--print-resource-dir"])
        .output()
        .expect("xcrun clang --print-resource-dir (Xcode command line tools required)");
    let resource_dir = String::from_utf8(output.stdout).expect("utf-8 path");
    let lib_dir = format!("{}/lib/darwin", resource_dir.trim());
    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}

/// Turns the `.rc` (which names the `.ico`) into an object file with windres and links it
/// into the program. Without windres the build goes on and the exe has the default icon.
fn link_icon() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let dir = manifest.join("../../packaging/windows");
    let rc = dir.join("ebobo.rc");
    println!("cargo:rerun-if-changed={}", rc.display());
    println!("cargo:rerun-if-changed={}", dir.join("ebobo.ico").display());
    let object = Path::new(&std::env::var("OUT_DIR").expect("set by cargo")).join("ebobo-icon.o");
    let status = Command::new("x86_64-w64-mingw32-windres")
        .arg("--include-dir")
        .arg(&dir)
        .args(["-O", "coff", "-i"])
        .arg(&rc)
        .arg("-o")
        .arg(&object)
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", object.display()),
        Ok(s) => println!("cargo:warning=windres: {s}; ebobo.exe gets no icon"),
        Err(e) => println!("cargo:warning=windres cannot be run ({e}); ebobo.exe gets no icon"),
    }
}
