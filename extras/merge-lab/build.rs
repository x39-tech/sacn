//! Builds and links the reference ETC sACN library for the `etc` feature.
//!
//! Only runs when the `etc` feature is enabled. It clones a pinned release of
//! ETC's sACN from GitHub into `$OUT_DIR`, then drives ETC's own CMake to
//! produce `libsACN.a` + `libEtcPal.a`, and links them.
//!
//! Overridable via environment:
//! - `SACN_C_DIR`   - use an existing local sACN checkout instead of cloning.
//! - `SACN_GIT_URL` / `SACN_GIT_REF` - clone a different repo/tag.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_GIT_URL: &str = "https://github.com/ETCLabs/sACN.git";
const DEFAULT_GIT_REF: &str = "7470a8063889cb7f24ed58494f715a40d471a073";

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {what}: {e}"));
    assert!(status.success(), "{what} failed");
}

fn main() {
    if env::var_os("CARGO_FEATURE_ETC").is_none() {
        return;
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SACN_C_DIR");
    println!("cargo:rerun-if-env-changed=SACN_GIT_URL");
    println!("cargo:rerun-if-env-changed=SACN_GIT_REF");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Source: an explicit local checkout, or a pinned shallow clone into OUT_DIR.
    let sacn_src = match env::var("SACN_C_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => clone_into(&out_dir),
    };

    let build_dir = out_dir.join("sacn-build");

    // CMake caches the absolute source path; if it changed (e.g. switching
    // between a local checkout and the pinned clone), the cache is unusable.
    // Reset the build dir when the source path differs from last time.
    let src_stamp = out_dir.join("sacn-build.src");
    let want_src = sacn_src.to_string_lossy().to_string();
    if std::fs::read_to_string(&src_stamp).unwrap_or_default() != want_src {
        let _ = std::fs::remove_dir_all(&build_dir);
        std::fs::write(&src_stamp, &want_src).expect("write build src stamp");
    }

    run(
        Command::new("cmake").args([
            "-S",
            sacn_src.to_str().unwrap(),
            "-B",
            build_dir.to_str().unwrap(),
            "-DCMAKE_BUILD_TYPE=Release",
            "-DBUILD_SHARED_LIBS=OFF",
        ]),
        "cmake configure",
    );
    run(
        Command::new("cmake").args([
            "--build",
            build_dir.to_str().unwrap(),
            "--config",
            "Release",
            "-j",
        ]),
        "cmake build",
    );

    println!("cargo:rustc-link-search=native={}/src", build_dir.display());
    println!(
        "cargo:rustc-link-search=native={}/_deps/etcpal-build/src",
        build_dir.display()
    );
    // MSVC multi-config generators place outputs in a config-named subdirectory.
    println!(
        "cargo:rustc-link-search=native={}/src/Release",
        build_dir.display()
    );
    println!(
        "cargo:rustc-link-search=native={}/_deps/etcpal-build/src/Release",
        build_dir.display()
    );
    println!("cargo:rustc-link-lib=static=sACN");
    println!("cargo:rustc-link-lib=static=EtcPal");
    // EtcPal depends on Windows system libraries for timers and network info.
    if cfg!(target_os = "windows") {
        println!("cargo:rustc-link-lib=dylib=winmm");
        println!("cargo:rustc-link-lib=dylib=iphlpapi");
    }
}

/// Shallow-clones the pinned sACN tag into `$OUT_DIR/sacn-src`
fn clone_into(out_dir: &Path) -> PathBuf {
    let url = env::var("SACN_GIT_URL").unwrap_or_else(|_| DEFAULT_GIT_URL.to_string());
    let git_ref = env::var("SACN_GIT_REF").unwrap_or_else(|_| DEFAULT_GIT_REF.to_string());
    let src = out_dir.join("sacn-src");

    // Re-clone if the pin changed or a previous clone is incomplete.
    let stamp = out_dir.join("sacn-src.ref");
    let current = std::fs::read_to_string(&stamp).unwrap_or_default();
    let want = format!("{url}@{git_ref}");
    if src.join("CMakeLists.txt").exists() && current == want {
        return src;
    }
    let _ = std::fs::remove_dir_all(&src);

    run(
        Command::new("git").args([
            "clone",
            "--depth",
            "1",
            "--revision",
            &git_ref,
            &url,
            src.to_str().unwrap(),
        ]),
        "git clone sACN",
    );
    std::fs::write(&stamp, &want).expect("write clone stamp");
    src
}
