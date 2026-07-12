//! Developer helper tasks for this repo, invoked as `cargo xtask <cmd>`.
//!
//! Currently just used for inspecting the compiled embassy example firmware
//! in examples/embassy.
//!
//! Commands:
//!   embassy-size        Flash/SRAM totals and per-section breakdown (`cargo size`).
//!   embassy-bloat [N]   Flash usage by crate and by symbol, top N (`cargo bloat`).
//!   embassy-ram   [N]   Static SRAM usage by symbol, top N (`cargo nm`, parsed here).

use std::cmp::Reverse;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const EMBASSY_EXAMPLE_DIR: &str = "examples/embassy";
const EMBASSY_EXAMPLE_BIN: &str = "sacn-embassy-example";

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    // Optional trailing count for `bloat`/`ram`.
    let count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(25);

    let ok = match cmd.as_str() {
        // The analysis commands all need the firmware built first.
        "embassy-size" => build_embassy_example() && cmd_size(),
        "embassy-bloat" => build_embassy_example() && cmd_embassy_bloat(count),
        "embassy-ram" => build_embassy_example() && cmd_ram(count),
        "" | "help" | "-h" | "--help" => {
            print_help();
            true
        }
        other => {
            eprintln!("xtask: unknown command `{other}`\n");
            print_help();
            false
        }
    };

    if !ok {
        std::process::exit(1);
    }
}

/// Absolute path to the example workspace, derived from this crate's location
fn embassy_example_dir() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate has a parent (the repo root)")
        .to_path_buf();
    root.join(EMBASSY_EXAMPLE_DIR)
}

// Strip host toolchain environment variables to honor the embedded
// rust-toolchain.toml.
fn embassy_ex_cargo_command(args: &[&str]) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(embassy_example_dir())
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("RUSTDOC")
        .args(args);
    cmd
}

fn build_embassy_example() -> bool {
    eprintln!("Building {EMBASSY_EXAMPLE_BIN} (release)...");
    match embassy_ex_cargo_command(&["build", "--release", "--bin", EMBASSY_EXAMPLE_BIN]).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("xtask: firmware build failed ({s})");
            false
        }
        Err(e) => {
            eprintln!("xtask: failed to spawn `cargo build`: {e}");
            false
        }
    }
}

/// Run a `cargo` subcommand and return its stdout.
fn capture(args: &[&str]) -> Option<String> {
    match embassy_ex_cargo_command(args)
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            eprint!("{}", String::from_utf8_lossy(&o.stderr));
            eprintln!("xtask: `cargo {}` failed", args.join(" "));
            None
        }
        Err(e) => {
            eprintln!("xtask: failed to spawn `cargo {}`: {e}", args.join(" "));
            None
        }
    }
}

fn cmd_size() -> bool {
    let Some(totals) = capture(&[
        "size",
        "--release",
        "--bin",
        EMBASSY_EXAMPLE_BIN,
        "--",
        "-B",
    ]) else {
        return false;
    };
    println!("== Flash & SRAM totals (bytes) ==");
    println!("    text = code + read-only data    (.text + .rodata + .vector_table) -> flash");
    println!("    data = initialized statics       (.data) -> flash image, copied to SRAM at boot");
    println!("    bss  = zero-initialized statics  (.bss)  -> SRAM only");
    println!("  flash used = text + data      static SRAM = data + bss");
    print!("{totals}");

    let Some(sections) = capture(&[
        "size",
        "--release",
        "--bin",
        EMBASSY_EXAMPLE_BIN,
        "--",
        "-A",
    ]) else {
        return false;
    };
    println!("\n== Section breakdown ==");
    print!("{sections}");
    true
}

fn cmd_embassy_bloat(n: usize) -> bool {
    let n = n.to_string();
    let Some(by_crate) = capture(&[
        "bloat",
        "--release",
        "--bin",
        EMBASSY_EXAMPLE_BIN,
        "--crates",
        "-n",
        &n,
    ]) else {
        return false;
    };
    println!("== Flash (.text) by crate ==\n");
    print!("{by_crate}");

    let Some(by_symbol) = capture(&["bloat", "--release", "--bin", EMBASSY_EXAMPLE_BIN, "-n", &n])
    else {
        return false;
    };
    println!("\n== Flash (.text) by symbol ==\n");
    print!("{by_symbol}");
    true
}

/// Static SRAM by symbol: `bss` (`b`/`B`) + `data` (`d`/`D`), largest first.
///
/// Replaces `cargo nm ... | awk '...' | sort -rn`. We capture `cargo nm`'s
/// output and do the filter/sort/format in Rust so it runs anywhere.
fn cmd_ram(n: usize) -> bool {
    let Some(text) = capture(&[
        "nm",
        "--release",
        "--bin",
        EMBASSY_EXAMPLE_BIN,
        "--",
        "--print-size",
        "--size-sort",
        "--radix=d",
    ]) else {
        return false;
    };

    let mut syms: Vec<(u64, char, String)> = text
        .lines()
        .filter_map(parse_nm_line)
        .filter(|(_, kind, _)| matches!(kind, 'b' | 'B' | 'd' | 'D'))
        .collect();

    // Largest first.
    syms.sort_by_key(|&(size, ..)| Reverse(size));

    println!("== Static SRAM by symbol (b/B = bss, d/D = data), top {n} ==");
    println!("{:>9}  {:<4}  symbol", "bytes", "kind");
    for (size, kind, name) in syms.iter().take(n) {
        let section = if matches!(kind, 'b' | 'B') {
            "bss"
        } else {
            "data"
        };
        println!("{size:>9}  {section:<4}  {name}");
    }

    let total: u64 = syms.iter().map(|(size, ..)| size).sum();
    println!(
        "\nsum across all {} bss+data symbols: {total} bytes \
         (approximate; run `cargo xtask size` for the exact SRAM figure)",
        syms.len()
    );
    true
}

/// Parse one `llvm-nm --print-size --size-sort --radix=d` line into
/// `(size, type, name)`. Lines look like: `<addr> <size> <type> <name...>`.
/// Names can contain spaces (e.g. `<impl Foo>`), so everything past the type is
/// the name.
fn parse_nm_line(line: &str) -> Option<(u64, char, String)> {
    let mut it = line.split_whitespace();
    let _addr = it.next()?;
    let size: u64 = it.next()?.parse().ok()?;
    let kind_str = it.next()?;
    // The type is a single character; skip anything unexpected.
    let mut chars = kind_str.chars();
    let kind = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let name: Vec<&str> = it.collect();
    if name.is_empty() {
        return None;
    }
    Some((size, kind, name.join(" ")))
}

fn print_help() {
    println!(
        "cargo xtask <command>

Inspect the compiled `{EMBASSY_EXAMPLE_BIN}` firmware.

Commands:
  embassy-size        Flash/SRAM totals and per-section breakdown.
  embassy-bloat [N]   Flash usage by crate and by symbol (top N, default 25).
  embassy-ram   [N]   Static SRAM usage by symbol (top N, default 25).

Examples:
  cargo xtask embassy-size
  cargo xtask embassy-bloat 30
  cargo xtask embassy-ram"
    );
}
