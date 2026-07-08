# Fuzzing the sACN codec

This directory is a standalone [`cargo-fuzz`] crate that fuzzes the protocol codec. It is intentionally detached from the main build (its `Cargo.toml` has an empty `[workspace]` table), so a normal `cargo build`/`cargo test` ignores it.

> Run every command below from the repository root; `cargo fuzz` locates this `fuzz/` directory itself, and the paths assume that working directory.

## Prerequisites

```sh
rustup toolchain install nightly          # cargo-fuzz needs -Zsanitizer
cargo install cargo-fuzz
```

cargo-fuzz builds with AddressSanitizer + SanitizerCoverage, both nightly-only, so every command below runs under `+nightly`.

## Running

The committed seeds are a read-only source of truth; copy them into the (git-ignored) live corpus that libFuzzer reads from and writes new finds into, then run:

```sh
mkdir -p fuzz/corpus/parse && cp fuzz/seed_corpus/parse/* fuzz/corpus/parse/
cargo +nightly fuzz run parse -- \
    -max_len=1144 -use_value_profile=1 -dict=fuzz/parse.dict
```

Why these flags: the strict 16-byte preamble makes random inputs rarely reach the framing parser, so the seeds + `fuzz/parse.dict` get libFuzzer past the fixed-byte gates; `-max_len=1144` is `MAX_PACKET_SIZE`; `-use_value_profile=1` helps it solve multi-byte comparisons. Add `-max_total_time=<secs>` or `-runs=<n>` to bound a run.

### Reproducing and minimizing a crash

```sh
cargo +nightly fuzz run parse fuzz/artifacts/parse/crash-<hash>   # reproduce
cargo +nightly fuzz fmt parse fuzz/artifacts/parse/crash-<hash>   # show decoded input
cargo +nightly fuzz tmin parse fuzz/artifacts/parse/crash-<hash>  # minimize
```

When you fix a crash, add the minimized input to `fuzz/seed_corpus/parse/` and, where practical, capture it as a unit test in `src/packet/tests.rs`.

## Coverage report

To see which regions of the codec the current corpus exercises:

```sh
# 1. Grow a corpus first (coverage is only as good as the corpus it runs).
mkdir -p fuzz/corpus/parse && cp fuzz/seed_corpus/parse/* fuzz/corpus/parse/
cargo +nightly fuzz run parse -- -max_total_time=60 -max_len=1144 \
    -use_value_profile=1 -dict=fuzz/parse.dict

# 2. Run the corpus under a source-based-coverage build.
rustup component add llvm-tools-preview --toolchain nightly   # once
cargo +nightly fuzz coverage parse

# 3. Render a summary for the codec source.
HOST=$(rustc -vV | sed -n 's/^host: //p')
LLVM_COV="$(rustc --print sysroot)/lib/rustlib/$HOST/bin/llvm-cov"
BIN="target/$HOST/coverage/$HOST/release/parse"
"$LLVM_COV" report "$BIN" \
    -instr-profile=fuzz/coverage/parse/coverage.profdata \
    -show-region-summary \
    src/packet/mod.rs src/packet/cursor.rs
```

Add `-Xdemangler=rustfilt` (`cargo install rustfilt`) plus `-show-functions
src/packet/mod.rs` to list per-function coverage, or swap `report` for `show ...
-format=html -output-dir=fuzz/coverage/html` to browse line-by-line.

This output should be considered informational only. Take it with a grain of salt and don't chase increased coverage too much.

In the case of the `parse` target, the codec logic (`cursor.rs`, the parse/serialize paths in `mod.rs`) should be near-fully covered; persistent gaps in _parser_ branches are worth a seed or a test.

[`cargo-fuzz`]: https://github.com/rust-fuzz/cargo-fuzz
