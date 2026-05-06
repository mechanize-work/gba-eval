//! Probe for the Mesen native static library at build time. If
//! `reference/build-mesen/native/libmesen.a` is present, compile the
//! C++ shim and set `cfg(mesen_available)` so `mod mesen` is
//! included. If absent (the normal case for fresh clones), the
//! cfg stays unset, the module is gated out, and the grader uses
//! the wasm reference at runtime.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../../reference/mesen_step.cpp");
    println!("cargo:rerun-if-changed=../../spec/gba_bios_stub.h");

    // Native shims don't make sense on wasm targets.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("wasm") {
        register_check_cfg();
        return;
    }

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.parent().and_then(|p| p.parent()).unwrap();

    build_mesen(root);

    // cc::Build emits single-colon directives. Cargo treats the first
    // `cargo::` as a mode switch and silently ignores single-colon
    // ones afterward. So check-cfg goes last.
    register_check_cfg();
}

fn register_check_cfg() {
    println!("cargo::rustc-check-cfg=cfg(mesen_available)");
}

fn build_mesen(root: &Path) {
    let mesen = root.join("third_party/mesen");
    let lib = root.join("reference/build-mesen/native/libmesen.a");
    let shim = root.join("reference/mesen_step.cpp");

    if !lib.exists() {
        return;
    }

    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rerun-if-changed={}", shim.display());

    // -fno-strict-aliasing is load-bearing: Mesen type-puns somewhere
    // and at -O2 without it, EmuSettings::GetOverscan reads _emu=null
    // and segfaults.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .file(&shim)
        .include(mesen.join("Core"))
        .include(&mesen)
        .include(mesen.join("Utilities"))
        .include(root.join("spec")) // gba_bios_stub.h
        .define("MESEN_HEADLESS", None)
        .define("MESEN_LIB_ONLY", None)
        .std("c++17")
        .flag("-fno-rtti")
        .flag("-fno-strict-aliasing")
        .opt_level(2)
        .warnings(false);

    if let Err(e) = build.try_compile("mesen_shim") {
        eprintln!("warning: failed to compile mesen_step.cpp: {e}");
        return;
    }

    println!("cargo:rustc-link-search=native={}", lib.parent().unwrap().display());
    println!("cargo:rustc-link-lib=static=mesen");
    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }

    println!("cargo:rustc-cfg=mesen_available");
}
