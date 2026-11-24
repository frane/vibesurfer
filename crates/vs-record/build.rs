//! Links libvpx and generates its bindings for WebM recording (the
//! `webm` feature). `VPX_LIB_DIR` / `VPX_INCLUDE_DIR` point at a build
//! of libvpx (static with `VPX_STATIC=1`, as release builds do);
//! otherwise pkg-config finds the system one.

use std::env;
use std::path::PathBuf;

fn main() {
    for v in ["VPX_LIB_DIR", "VPX_INCLUDE_DIR", "VPX_STATIC"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    if env::var_os("CARGO_FEATURE_WEBM").is_none() {
        return;
    }
    let includes = link();
    let mut builder = bindgen::Builder::default()
        .header_contents(
            "vpx.h",
            "#include <vpx/vpx_encoder.h>\n#include <vpx/vp8cx.h>\n",
        )
        .allowlist_function("vpx_.*")
        .allowlist_type("vpx_.*|vp8e_.*")
        .allowlist_var("VPX_.*")
        .rustified_enum("vpx_.*|vp8e_.*")
        .layout_tests(false)
        .generate_comments(false);
    for dir in &includes {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    builder
        .generate()
        .expect("bindgen: libvpx headers (install libvpx, or set VPX_INCLUDE_DIR)")
        .write_to_file(out.join("vpx.rs"))
        .expect("write vpx.rs");
}

/// Emit the link flags; return the include directories.
fn link() -> Vec<PathBuf> {
    let Some(lib) = env::var_os("VPX_LIB_DIR") else {
        let found = pkg_config::Config::new()
            .probe("vpx")
            .expect("libvpx not found: install it (brew install libvpx, apt install libvpx-dev) or set VPX_LIB_DIR");
        return found.include_paths;
    };
    println!(
        "cargo:rustc-link-search=native={}",
        PathBuf::from(lib).display()
    );
    let windows = env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let name = if windows { "libvpx" } else { "vpx" };
    if env::var_os("VPX_STATIC").is_some() {
        println!("cargo:rustc-link-lib=static={name}");
    } else {
        println!("cargo:rustc-link-lib={name}");
    }
    env::var_os("VPX_INCLUDE_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect()
}
