//! Public-API snapshot test.
//!
//! Builds the crate's public API via rustdoc JSON and compares it against the
//! committed snapshot at `tests/snapshots/public-api.txt`, so every change to
//! the public API shows up as a reviewable diff.
//!
//! This test needs a nightly toolchain (rustdoc JSON output is nightly-only) and
//! is slow, so it is gated at runtime: it does nothing unless the
//! `RUN_PUBLIC_API_TEST` env var is set. A normal `cargo test` compiles but skips
//! it. With nightly installed, run it with:
//!
//! ```text
//! RUN_PUBLIC_API_TEST=1 cargo test --test public_api
//! ```
//!
//! To accept an intentional API change, regenerate the snapshot with:
//!
//! ```text
//! RUN_PUBLIC_API_TEST=1 UPDATE_SNAPSHOTS=yes cargo test --test public_api
//! ```

#[test]
fn public_api_matches_snapshot() {
    if std::env::var_os("RUN_PUBLIC_API_TEST").is_none() {
        eprintln!("skipping public_api test; set RUN_PUBLIC_API_TEST=1 to run it");
        return;
    }

    let toolchain = std::env::var("PUBLIC_API_TOOLCHAIN").unwrap_or_else(|_| "nightly".to_string());

    let rustdoc_json = rustdoc_json::Builder::default()
        .toolchain(toolchain)
        .build()
        .expect("failed to build rustdoc JSON; a nightly toolchain is required");

    let public_api = public_api::Builder::from_rustdoc_json(rustdoc_json)
        .build()
        .expect("failed to parse the public API");

    public_api.assert_eq_or_update("tests/snapshots/public-api.txt");
}
